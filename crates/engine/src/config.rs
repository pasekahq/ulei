//! Settings of `ulei-engine run`: each comes from its environment variable, else from the
//! config file. The file is read in a strict subset of TOML (section headers, `key = "string"`,
//! `key = integer`, comments, blank lines); any other line is an error naming the line.

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// Environment variable naming the config file when no `--config` flag is given.
pub const CONFIG_ENV: &str = "ULEI_CONFIG";

/// A setting: its dotted key in the config file and the environment variable overriding it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Setting {
    /// `section.key` in the config file.
    pub key: &'static str,
    /// Environment variable that wins over the file.
    pub env: &'static str,
}

/// Unix socket of the qb gateway.
pub const QB_SOCKET: Setting = Setting {
    key: "qb.socket",
    env: "ULEI_QB_SOCKET",
};

/// The session journal file.
pub const JOURNAL: Setting = Setting {
    key: "engine.journal",
    env: "ULEI_ENGINE_JOURNAL",
};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Value {
    /// `key = "text"`.
    String(String),
    /// `key = 42`.
    Integer(i64),
}

/// Why the settings could not be resolved.
#[derive(Debug)]
pub enum Error {
    /// The config file could not be read.
    Read(PathBuf, io::Error),
    /// A line of the config file is outside the supported subset.
    Line {
        /// The config file.
        path: PathBuf,
        /// Line number, from 1.
        line: usize,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// Neither the environment variable nor the config file sets the value.
    Missing(Setting),
    /// The value is set but empty.
    Empty(Setting),
    /// The config file sets the value to an integer where a string is required.
    NotString(Setting),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(path, e) => write!(f, "config {}: {e}", path.display()),
            Self::Line { path, line, reason } => {
                write!(f, "config {} line {line}: {reason}", path.display())
            }
            Self::Missing(s) => write!(
                f,
                "missing {}: set it in the config file or in {}",
                s.key, s.env
            ),
            Self::Empty(s) => write!(f, "{} ({}) is empty", s.key, s.env),
            Self::NotString(s) => write!(f, "{} in the config file must be a string", s.key),
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

/// The values of a config file by dotted key, or none without a file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    values: Vec<(String, Value)>,
}

impl Config {
    /// Reads the config file at `path`.
    ///
    /// # Errors
    /// [`Error::Read`], or [`Error::Line`] for the first line outside the subset.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path).map_err(|e| Error::Read(path.to_owned(), e))?;
        parse(&text).map_err(|(line, reason)| Error::Line {
            path: path.to_owned(),
            line,
            reason,
        })
    }

    fn get(&self, key: &str) -> Option<&Value> {
        self.values.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// The path `setting` names: `env` when set, else the file's string value.
    ///
    /// # Errors
    /// [`Error::Missing`], [`Error::Empty`] or [`Error::NotString`].
    pub fn path(&self, setting: Setting, env: Option<OsString>) -> Result<PathBuf, Error> {
        let value = match (env, self.get(setting.key)) {
            (Some(value), _) => value,
            (None, Some(Value::String(value))) => OsString::from(value),
            (None, Some(Value::Integer(_))) => return Err(Error::NotString(setting)),
            (None, None) => return Err(Error::Missing(setting)),
        };
        if value.is_empty() {
            return Err(Error::Empty(setting));
        }
        Ok(PathBuf::from(value))
    }
}

/// The values of `text`, or the first offending line number and why.
fn parse(text: &str) -> Result<Config, (usize, &'static str)> {
    let mut values: Vec<(String, Value)> = Vec::new();
    let mut sections: Vec<String> = Vec::new();
    let mut section = String::new();
    for (n, line) in (1..).zip(text.lines()) {
        let line = line.trim_matches([' ', '\t']);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            let (name, rest) = header
                .split_once(']')
                .ok_or((n, "unterminated section header"))?;
            let name = name.trim_matches([' ', '\t']);
            if !name.split('.').all(is_bare_key) {
                return Err((n, "unsupported section header"));
            }
            trailing(rest).ok_or((n, "unexpected text after the section header"))?;
            if sections.iter().any(|s| s == name) {
                return Err((n, "duplicate section"));
            }
            sections.push(name.to_owned());
            name.clone_into(&mut section);
            continue;
        }
        let (key, value) = line.split_once('=').ok_or((n, "expected key = value"))?;
        let key = key.trim_matches([' ', '\t']);
        if !is_bare_key(key) {
            return Err((n, "unsupported key"));
        }
        let value = parse_value(value.trim_matches([' ', '\t'])).map_err(|reason| (n, reason))?;
        let key = if section.is_empty() {
            key.to_owned()
        } else {
            format!("{section}.{key}")
        };
        if values.iter().any(|(k, _)| *k == key) {
            return Err((n, "duplicate key"));
        }
        values.push((key, value));
    }
    Ok(Config { values })
}

/// A basic string without escapes or a decimal integer, then at most a comment.
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
