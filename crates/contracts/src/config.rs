//! The decoder of `ulei.toml` (C9): finds the file, reads it in a strict subset of TOML
//! (section headers, `key = "string"` without escapes, `key = integer`, `key = true` or `false`,
//! comments, blank lines) and checks every section and key against [`c9::KEYS`].
//! An error is one line naming the file and the line, never a value.

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::generated::c9::{self, Kind};

/// Environment variable naming the file when no `--config` flag is given.
pub const CONFIG_ENV: &str = "ULEI_CONFIG";

/// A value of the subset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// `key = "text"`.
    String(String),
    /// `key = 42`.
    Integer(i64),
    /// `key = true` or `false`.
    Bool(bool),
}

/// One assignment of the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The dotted key, `section.key`.
    pub key: String,
    /// The line it is on, from 1.
    pub line: usize,
    /// Its value, of the kind the catalogue gives the key.
    pub value: Value,
}

/// One section header of the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// The dotted name between the brackets.
    pub name: String,
    /// The line of the header, from 1.
    pub line: usize,
}

/// The values of a config file by dotted key, in file order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The file read.
    pub path: PathBuf,
    /// Every assignment, each key once.
    pub entries: Vec<Entry>,
    /// Every section header, each name once, with or without keys under it.
    pub sections: Vec<Section>,
}

/// What is wrong with a line of the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// The line is outside the subset, for this reason.
    Syntax(&'static str),
    /// The section is not in the catalogue.
    UnknownSection(String),
    /// The dotted key is not in the catalogue.
    UnknownKey(String),
    /// The dotted key holds a value of another kind than this one.
    WrongKind(String, Kind),
}

/// Why the file could not be found, read or decoded.
#[derive(Debug)]
pub enum Error {
    /// No `--config`, and none of the variables that locate the file is set.
    NoPath,
    /// The file could not be read.
    Read(PathBuf, io::Error),
    /// A line of the file is refused.
    Line {
        /// The file.
        path: PathBuf,
        /// Line number, from 1.
        line: usize,
        /// What is wrong with it.
        fault: Fault,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoPath => write!(
                f,
                "config: no --config and none of {CONFIG_ENV}, XDG_CONFIG_HOME or HOME is set"
            ),
            Self::Read(path, e) => write!(f, "config {}: {e}", path.display()),
            Self::Line { path, line, fault } => {
                write!(f, "config {} line {line}: ", path.display())?;
                match fault {
                    Fault::Syntax(reason) => f.write_str(reason),
                    Fault::UnknownSection(name) => write!(f, "unknown section [{name}]"),
                    Fault::UnknownKey(key) => write!(f, "unknown key {key}"),
                    Fault::WrongKind(key, kind) => write!(f, "{key} must be {}", kind_text(*kind)),
                }
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(_, e) => Some(e),
            _ => None,
        }
    }
}

impl Config {
    /// The file found by [`Config::find`] in the process environment, decoded.
    ///
    /// # Errors
    /// As [`Config::find`] and [`Config::load`].
    pub fn read(flag: Option<&Path>) -> Result<Self, Error> {
        Self::load(&Self::find(flag, |name| std::env::var_os(name))?)
    }

    /// The path of the file: `flag`, else [`CONFIG_ENV`], else `$XDG_CONFIG_HOME/ulei/ulei.toml`
    /// when it is absolute, else `$HOME/.config/ulei/ulei.toml`. `env` reads a variable; an
    /// empty one counts as unset.
    ///
    /// # Errors
    /// [`Error::NoPath`] when no step applies.
    pub fn find(
        flag: Option<&Path>,
        env: impl Fn(&str) -> Option<OsString>,
    ) -> Result<PathBuf, Error> {
        let var = |name| env(name).filter(|v| !v.is_empty()).map(PathBuf::from);
        if let Some(path) = flag {
            return Ok(path.to_owned());
        }
        if let Some(path) = var(CONFIG_ENV) {
            return Ok(path);
        }
        if let Some(dir) = var("XDG_CONFIG_HOME").filter(|d| d.is_absolute()) {
            return Ok(dir.join("ulei/ulei.toml"));
        }
        var("HOME")
            .map(|home| home.join(".config/ulei/ulei.toml"))
            .ok_or(Error::NoPath)
    }

    /// Reads and decodes the file at `path`.
    ///
    /// # Errors
    /// [`Error::Read`], or [`Error::Line`] for the first line refused.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path).map_err(|e| Error::Read(path.to_owned(), e))?;
        let (entries, sections) = parse(&text).map_err(|(line, fault)| Error::Line {
            path: path.to_owned(),
            line,
            fault,
        })?;
        Ok(Self {
            path: path.to_owned(),
            entries,
            sections,
        })
    }

    /// The entry of the dotted `key`.
    pub fn get(&self, key: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.key == key)
    }
}

fn kind_text(kind: Kind) -> &'static str {
    match kind {
        Kind::String => "a string",
        Kind::Integer => "an integer",
        Kind::Boolean => "true or false",
        Kind::IntegerOrString => "an integer or a string",
        Kind::Any => "a string, an integer or a boolean",
    }
}

/// The entries and section headers of `text`, or the first refused line number and why.
fn parse(text: &str) -> Result<(Vec<Entry>, Vec<Section>), (usize, Fault)> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut sections: Vec<Section> = Vec::new();
    let mut section = String::new();
    for (n, line) in (1..).zip(text.lines()) {
        let syntax = |reason| (n, Fault::Syntax(reason));
        let line = line.trim_matches([' ', '\t']);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            let (name, rest) = header
                .split_once(']')
                .ok_or(syntax("unterminated section header"))?;
            let name = name.trim_matches([' ', '\t']);
            if !name.split('.').all(is_bare_key) {
                return Err(syntax("unsupported section header"));
            }
            trailing(rest).ok_or(syntax("unexpected text after the section header"))?;
            if sections.iter().any(|s| s.name == name) {
                return Err(syntax("duplicate section"));
            }
            if !is_section(name) {
                return Err((n, Fault::UnknownSection(name.to_owned())));
            }
            sections.push(Section {
                name: name.to_owned(),
                line: n,
            });
            name.clone_into(&mut section);
            continue;
        }
        let (key, value) = line.split_once('=').ok_or(syntax("expected key = value"))?;
        let key = key.trim_matches([' ', '\t']);
        if !is_bare_key(key) {
            return Err(syntax("unsupported key"));
        }
        let value = parse_value(value.trim_matches([' ', '\t'])).map_err(syntax)?;
        let key = if section.is_empty() {
            key.to_owned()
        } else {
            format!("{section}.{key}")
        };
        if entries.iter().any(|e| e.key == key) {
            return Err(syntax("duplicate key"));
        }
        let Some(kind) = kind_of(&key) else {
            return Err((n, Fault::UnknownKey(key)));
        };
        if !fits(&value, kind) {
            return Err((n, Fault::WrongKind(key, kind)));
        }
        entries.push(Entry {
            key,
            line: n,
            value,
        });
    }
    Ok((entries, sections))
}

/// A basic string without escapes, a decimal integer or a boolean, then at most a comment.
fn parse_value(text: &str) -> Result<Value, &'static str> {
    if let Some(body) = text.strip_prefix('"') {
        let (string, rest) = body.split_once('"').ok_or("unterminated string")?;
        if string.contains('\\') {
            return Err("escapes in strings are not supported");
        }
        if string.chars().any(char::is_control) {
            return Err("control character in a string");
        }
        trailing(rest).ok_or("unexpected text after the value")?;
        return Ok(Value::String(string.to_owned()));
    }
    let end = text.find([' ', '\t', '#']).unwrap_or(text.len());
    let (number, rest) = text.split_at(end);
    if let Some(value) = match number {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    } {
        trailing(rest).ok_or("unexpected text after the value")?;
        return Ok(Value::Bool(value));
    }
    let digits = number.strip_prefix(['+', '-']).unwrap_or(number);
    let decimal = !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && (digits == "0" || !digits.starts_with('0'));
    if !decimal {
        return Err("unsupported value");
    }
    trailing(rest).ok_or("unexpected text after the value")?;
    number
        .parse()
        .map(Value::Integer)
        .map_err(|_| "integer out of range")
}

/// `Some` when `rest` is blank or a comment.
fn trailing(rest: &str) -> Option<()> {
    let rest = rest.trim_matches([' ', '\t']);
    (rest.is_empty() || rest.starts_with('#')).then_some(())
}

fn is_bare_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Whether `name` segment by segment is a proper prefix of `pattern`, `*` matching any segment.
fn prefixes(pattern: &str, name: &str) -> bool {
    let mut pattern = pattern.split('.');
    name.split('.')
        .all(|seg| pattern.next().is_some_and(|p| p == "*" || p == seg))
        && pattern.next().is_some()
}

fn is_section(name: &str) -> bool {
    c9::KEYS.iter().any(|(pattern, _)| prefixes(pattern, name))
}

fn kind_of(key: &str) -> Option<Kind> {
    c9::KEYS.iter().find_map(|&(pattern, kind)| {
        (pattern.split('.').count() == key.split('.').count()
            && pattern
                .split('.')
                .zip(key.split('.'))
                .all(|(p, k)| p == "*" || p == k))
        .then_some(kind)
    })
}

fn fits(value: &Value, kind: Kind) -> bool {
    matches!(
        (kind, value),
        (
            Kind::String | Kind::IntegerOrString | Kind::Any,
            Value::String(_)
        ) | (
            Kind::Integer | Kind::IntegerOrString | Kind::Any,
            Value::Integer(_)
        ) | (Kind::Boolean | Kind::Any, Value::Bool(_))
    )
}
