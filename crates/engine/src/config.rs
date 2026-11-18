// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Settings of `ulei-engine run`: each comes from its environment variable, else from the
//! config file. The file is read in a strict subset of TOML (section headers, `key = "string"`,
//! `key = integer`, `key = true` or `false`, comments, blank lines); any other line is an error
//! naming the line.

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

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

/// Directory of the files holding the full output of `terminal` calls whose result was cut.
pub const TERMINAL_OUTPUT: Setting = Setting {
    key: "engine.terminal_output",
    env: "ULEI_ENGINE_TERMINAL_OUTPUT",
};

/// Deadline in seconds of one parallel segment of tool calls; `0` or less waits for every call.
pub const CONCURRENT_TOOL_TIMEOUT: Setting = Setting {
    key: "engine.concurrent_tool_timeout",
    env: "ULEI_ENGINE_CONCURRENT_TOOL_TIMEOUT",
};

/// Most steps of one turn before the summary request: a count, `0` or less, `none`, `null`,
/// `unlimited`, `infinite`, `infinity`, `inf` or `∞` for no limit.
pub const MAX_STEPS: Setting = Setting {
    key: "engine.max_steps",
    env: "ULEI_ENGINE_MAX_STEPS",
};

/// Whether two empty replies with zero output tokens from one route end the empty-reply
/// retries early; on unless set to another value than `1`, `true`, `yes` or `on`.
pub const EMPTY_RESPONSE_GUARD: Setting = Setting {
    key: "engine.empty_response_guard.enabled",
    env: "ULEI_ENGINE_EMPTY_RESPONSE_GUARD_ENABLED",
};

/// The systemd service unit running the engine, whose restart, stop or kill `terminal` refuses;
/// found from the process's cgroup under systemd when unset.
pub const SERVICE_UNIT: Setting = Setting {
    key: "engine.service_unit",
    env: "ULEI_ENGINE_SERVICE_UNIT",
};

/// Spellings of a step limit that mean no limit, compared in lowercase.
const UNLIMITED: [&str; 7] = [
    "none",
    "null",
    "unlimited",
    "infinite",
    "infinity",
    "inf",
    "∞",
];

/// Longest deadline handed to a wait; a longer one is cut to it.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(365 * 24 * 3600);

/// Whether tool output is redacted of secrets; on unless set to another value than `1`,
/// `true`, `yes` or `on`.
pub const REDACT_SECRETS: Setting = Setting {
    key: "security.redact_secrets",
    env: "ULEI_SECURITY_REDACT_SECRETS",
};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Value {
    /// `key = "text"`.
    String(String),
    /// `key = 42`.
    Integer(i64),
    /// `key = true`.
    Bool(bool),
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
    /// The value is not a number of seconds.
    NotSeconds(Setting),
    /// The value is not a step limit.
    NotLimit(Setting),
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
            Self::NotSeconds(s) => write!(f, "{} ({}) must be a number of seconds", s.key, s.env),
            Self::NotLimit(s) => write!(
                f,
                "{} ({}) must be a number of steps or unlimited",
                s.key, s.env
            ),
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
            (None, Some(Value::Integer(_) | Value::Bool(_))) => {
                return Err(Error::NotString(setting));
            }
            (None, None) => return Err(Error::Missing(setting)),
        };
        if value.is_empty() {
            return Err(Error::Empty(setting));
        }
        Ok(PathBuf::from(value))
    }

    /// The text `setting` names: `env` when set, else the file's string value, else `None`.
    ///
    /// # Errors
    /// [`Error::Empty`] or [`Error::NotString`].
    pub fn text(&self, setting: Setting, env: Option<OsString>) -> Result<Option<String>, Error> {
        let value = match (env, self.get(setting.key)) {
            (Some(value), _) => value.to_string_lossy().into_owned(),
            (None, Some(Value::String(value))) => value.clone(),
            (None, Some(Value::Integer(_) | Value::Bool(_))) => {
                return Err(Error::NotString(setting));
            }
            (None, None) => return Ok(None),
        };
        if value.is_empty() {
            return Err(Error::Empty(setting));
        }
        Ok(Some(value))
    }

    /// The deadline `setting` names: `env` when set and not blank, else the file's value, else
    /// `default`. A value is a decimal number of seconds, `inf` or `nan`; one of `0` or less or
    /// `nan` means no deadline, and one above [`MAX_TIMEOUT`] is cut to it.
    ///
    /// # Errors
    /// [`Error::NotSeconds`] for a value that is not such a number.
    pub fn timeout(
        &self,
        setting: Setting,
        env: Option<OsString>,
        default: Duration,
    ) -> Result<Option<Duration>, Error> {
        let env = env.filter(|value| !value.to_string_lossy().trim().is_empty());
        let seconds = match (env, self.get(setting.key)) {
            (Some(value), _) => seconds(&value.to_string_lossy()),
            (None, Some(Value::String(value))) => seconds(value),
            #[allow(clippy::cast_precision_loss)]
            (None, Some(Value::Integer(value))) => Some(*value as f64),
            (None, Some(Value::Bool(_))) => None,
            (None, None) => Some(default.as_secs_f64()),
        };
        let seconds = seconds.ok_or(Error::NotSeconds(setting))?;
        if seconds.is_nan() || seconds <= 0.0 {
            return Ok(None);
        }
        Ok(Some(
            Duration::try_from_secs_f64(seconds).map_or(MAX_TIMEOUT, |d| d.min(MAX_TIMEOUT)),
        ))
    }

    /// The step limit `setting` names: `env` when set and not blank, else the file's value,
    /// else `default`; a blank string in the file is `default`. A value is an integer or a
    /// decimal text whose fraction is cut; one of `0` or less, or an unlimited spelling of
    /// [`MAX_STEPS`], is [`usize::MAX`].
    ///
    /// # Errors
    /// [`Error::NotLimit`] for a value that is none of these.
    pub fn limit(
        &self,
        setting: Setting,
        env: Option<OsString>,
        default: usize,
    ) -> Result<usize, Error> {
        let env = env.filter(|value| !value.to_string_lossy().trim().is_empty());
        let count = match (env, self.get(setting.key)) {
            (Some(value), _) => limit_text(&value.to_string_lossy()),
            (None, Some(Value::String(value))) if value.trim().is_empty() => return Ok(default),
            (None, Some(Value::String(value))) => limit_text(value),
            (None, Some(Value::Integer(value))) => Some(*value),
            (None, Some(Value::Bool(_))) => None,
            (None, None) => return Ok(default),
        };
        let count = count.ok_or(Error::NotLimit(setting))?;
        Ok(if count <= 0 {
            usize::MAX
        } else {
            usize::try_from(count).unwrap_or(usize::MAX)
        })
    }

    /// The switch `setting` names: `env` when set, else the file's value, else on. A value is
    /// on when its lowercase text is `1`, `true`, `yes` or `on`.
    pub fn switch(&self, setting: Setting, env: Option<OsString>) -> bool {
        let text = match (env, self.get(setting.key)) {
            (Some(value), _) => value.to_string_lossy().into_owned(),
            (None, Some(Value::Bool(value))) => return *value,
            (None, Some(Value::String(value))) => value.clone(),
            (None, Some(Value::Integer(value))) => value.to_string(),
            (None, None) => return true,
        };
        ["1", "true", "yes", "on"].contains(&text.to_lowercase().as_str())
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

/// `text` as a number of seconds, surrounding whitespace ignored.
fn seconds(text: &str) -> Option<f64> {
    text.trim().parse().ok()
}

/// A step limit text as an integer, `0` for an unlimited spelling; `None` when it is no number.
fn limit_text(text: &str) -> Option<i64> {
    let text = text.trim().to_lowercase();
    if UNLIMITED.contains(&text.as_str()) {
        return Some(0);
    }
    if let Ok(n) = text.parse() {
        return Some(n);
    }
    let n: f64 = text.parse().ok().filter(|n: &f64| n.is_finite())?;
    #[allow(
        clippy::cast_possible_truncation,
        reason = "a saturating cut of the fraction"
    )]
    Some(n.trunc() as i64)
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
