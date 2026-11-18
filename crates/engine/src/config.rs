// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Settings of `ulei-engine run` and `serve`: each comes from its environment variable, else
//! from the config file. The file is read in a strict subset of TOML (section headers,
//! `key = "string"`, `key = integer`, `key = true` or `false`, comments, blank lines); any other
//! line is an error naming the line.

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

/// A setting of each named item of a section: `<section>.<name>.<field>` in the config file,
/// `ULEI_<SECTION>_<NAME>_<FIELD>` in the environment, uppercase with every character other
/// than a letter or digit as `_`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemSetting {
    /// The section holding one subsection per item.
    pub section: &'static str,
    /// The key under the item's subsection.
    pub field: &'static str,
}

impl ItemSetting {
    /// The dotted key of item `name` in the config file.
    pub fn key(self, name: &str) -> String {
        format!("{}.{name}.{}", self.section, self.field)
    }

    /// The environment variable of item `name`.
    pub fn env(self, name: &str) -> String {
        format!("ULEI_{}_{name}_{}", self.section, self.field)
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect()
    }

    /// The error `fault` of item `name`.
    pub fn error(self, name: &str, fault: Fault) -> Error {
        Error::Item {
            key: self.key(name),
            env: self.env(name),
            fault,
        }
    }
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

/// Unix socket on which `ulei-engine serve` serves C6.
pub const C6_SOCKET: Setting = Setting {
    key: "engine.c6_socket",
    env: "ULEI_ENGINE_C6_SOCKET",
};

/// The session key of the owner's session, which C6 `is_idle` reports on.
pub const OWNER_SESSION: Setting = Setting {
    key: "engine.owner_session",
    env: "ULEI_ENGINE_OWNER_SESSION",
};

/// The route alias the turns of `ulei-engine serve` run on.
pub const DEFAULT_ROUTE: Setting = Setting {
    key: "engine.default_route",
    env: "ULEI_ENGINE_DEFAULT_ROUTE",
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

/// SOUL.md, the identity at the head of the system prompt.
pub const SOUL_FILE: Setting = Setting {
    key: "engine.soul_file",
    env: "ULEI_ENGINE_SOUL_FILE",
};

/// INDEX.md of the knowledge base, which the system prompt points to.
pub const INDEX_FILE: Setting = Setting {
    key: "engine.index_file",
    env: "ULEI_ENGINE_INDEX_FILE",
};

/// MEMORY.md, the agent's notes rendered into the system prompt.
pub const MEMORY_FILE: Setting = Setting {
    key: "engine.memory_file",
    env: "ULEI_ENGINE_MEMORY_FILE",
};

/// USER.md, the profile of the user rendered into the system prompt.
pub const USER_FILE: Setting = Setting {
    key: "engine.user_file",
    env: "ULEI_ENGINE_USER_FILE",
};

/// Char limit of the MEMORY store shown in its block header.
pub const MEMORY_CHAR_LIMIT: Setting = Setting {
    key: "engine.memory_char_limit",
    env: "ULEI_ENGINE_MEMORY_CHAR_LIMIT",
};

/// Char limit of the USER store shown in its block header.
pub const USER_CHAR_LIMIT: Setting = Setting {
    key: "engine.user_char_limit",
    env: "ULEI_ENGINE_USER_CHAR_LIMIT",
};

/// The skills directory: the system prompt indexes it, `skills_list` and `skill_view` read it,
/// and C6 `skills_dir` names it.
pub const SKILLS_DIR: Setting = Setting {
    key: "engine.skills_dir",
    env: "ULEI_ENGINE_SKILLS_DIR",
};

/// Comma-separated names of the skills the index, `skills_list` and `skill_view` leave out.
pub const DISABLED_SKILLS: Setting = Setting {
    key: "engine.disabled_skills",
    env: "ULEI_ENGINE_DISABLED_SKILLS",
};

/// IANA time zone of the `Conversation started` line; the host's local time when unset.
pub const TIMEZONE: Setting = Setting {
    key: "engine.timezone",
    env: "ULEI_ENGINE_TIMEZONE",
};

/// The `ulei-web` helper binary; the web tools are registered only when it is set.
pub const WEB_HELPER: Setting = Setting {
    key: "engine.web_helper",
    env: "ULEI_ENGINE_WEB_HELPER",
};

/// Base URL of the Exa API, an `https://` URL; required with [`WEB_HELPER`].
pub const WEB_BASE_URL: Setting = Setting {
    key: "engine.web_base_url",
    env: "ULEI_ENGINE_WEB_BASE_URL",
};

/// The env-file holding `EXA_API_KEY`; required with [`WEB_HELPER`].
pub const WEB_ENV_FILE: Setting = Setting {
    key: "engine.web_env_file",
    env: "ULEI_ENGINE_WEB_ENV_FILE",
};

/// Directory of the full texts of pages `web_extract` truncated; required with [`WEB_HELPER`].
pub const WEB_STORE: Setting = Setting {
    key: "engine.web_store",
    env: "ULEI_ENGINE_WEB_STORE",
};

/// Per-page character budget of a `web_extract` call that names none, clamped to 2000-500000.
pub const WEB_EXTRACT_CHAR_LIMIT: Setting = Setting {
    key: "engine.web_extract_char_limit",
    env: "ULEI_ENGINE_WEB_EXTRACT_CHAR_LIMIT",
};

/// The channel whose owner turns are top-level owner turns; unset, no turn is one.
pub const MAIN_CHANNEL: Setting = Setting {
    key: "engine.main_channel",
    env: "ULEI_ENGINE_MAIN_CHANNEL",
};

/// The file whose text `turn-reminder` appends to the owner's input; set, the policy is on.
pub const TURN_REMINDER_FILE: Setting = Setting {
    key: "engine.turn_reminder_file",
    env: "ULEI_ENGINE_TURN_REMINDER_FILE",
};

/// Route alias of the `no-ask-guard` classifier; set, the policy is on and needs the note,
/// instructions and log settings.
pub const NO_ASK_GUARD_ROUTE: Setting = Setting {
    key: "engine.no_ask_guard.route",
    env: "ULEI_ENGINE_NO_ASK_GUARD_ROUTE",
};

/// The file of the `no-ask-guard` continuation note, with a `{chain}` placeholder.
pub const NO_ASK_GUARD_NOTE_FILE: Setting = Setting {
    key: "engine.no_ask_guard.note_file",
    env: "ULEI_ENGINE_NO_ASK_GUARD_NOTE_FILE",
};

/// The file of the `no-ask-guard` classifier instructions, with `{tag}`, `{owner}` and `{reply}`
/// placeholders.
pub const NO_ASK_GUARD_INSTRUCTIONS_FILE: Setting = Setting {
    key: "engine.no_ask_guard.instructions_file",
    env: "ULEI_ENGINE_NO_ASK_GUARD_INSTRUCTIONS_FILE",
};

/// The JSON-lines file of the `no-ask-guard` records.
pub const NO_ASK_GUARD_LOG: Setting = Setting {
    key: "engine.no_ask_guard.log",
    env: "ULEI_ENGINE_NO_ASK_GUARD_LOG",
};

/// Deadline in seconds of one `no-ask-guard` classifier call; `0` or less waits without one.
pub const NO_ASK_GUARD_TIMEOUT: Setting = Setting {
    key: "engine.no_ask_guard.timeout",
    env: "ULEI_ENGINE_NO_ASK_GUARD_TIMEOUT",
};

/// Route alias of the compaction summary turn.
pub const COMPRESSION_ROUTE: Setting = Setting {
    key: "engine.compression.route",
    env: "ULEI_ENGINE_COMPRESSION_ROUTE",
};

/// Deadline in seconds of the compaction summary turn; `0` or less waits without one.
pub const COMPRESSION_TIMEOUT: Setting = Setting {
    key: "engine.compression.timeout",
    env: "ULEI_ENGINE_COMPRESSION_TIMEOUT",
};

/// Recent messages a compaction keeps at least, up to 8.
pub const COMPRESSION_PROTECT_LAST_N: Setting = Setting {
    key: "engine.compression.protect_last_n",
    env: "ULEI_ENGINE_COMPRESSION_PROTECT_LAST_N",
};

/// Share of the compaction threshold kept as the verbatim tail, a decimal number.
pub const COMPRESSION_TARGET_RATIO: Setting = Setting {
    key: "engine.compression.target_ratio",
    env: "ULEI_ENGINE_COMPRESSION_TARGET_RATIO",
};

/// Share of a route's window at which compaction runs, a decimal number.
pub const COMPRESSION_THRESHOLD: Setting = Setting {
    key: "engine.compression.threshold",
    env: "ULEI_ENGINE_COMPRESSION_THRESHOLD",
};

/// Prompt tokens at which compaction runs at the latest; `0` sets no cap.
pub const COMPRESSION_THRESHOLD_TOKENS: Setting = Setting {
    key: "engine.compression.threshold_tokens",
    env: "ULEI_ENGINE_COMPRESSION_THRESHOLD_TOKENS",
};

/// Context length in tokens of a route alias; unset, no compaction runs before a step.
pub const ROUTE_CONTEXT_LENGTH: ItemSetting = ItemSetting {
    section: "engine.routes",
    field: "context_length",
};

/// Output tokens a step on a route alias may take, which its window reserves.
pub const ROUTE_MAX_TOKENS: ItemSetting = ItemSetting {
    section: "engine.routes",
    field: "max_tokens",
};

/// Whether `tool-guardrails` appends the identical-call notice and replaces a repeated result
/// with a reference stub; on unless set to another value than `1`, `true`, `yes` or `on`.
pub const STALL_GUARDS: Setting = Setting {
    key: "engine.stall_guards",
    env: "ULEI_ENGINE_STALL_GUARDS",
};

/// Whether `tool-guardrails` appends its warnings to tool results; on unless set to another
/// value than `1`, `true`, `yes` or `on`.
pub const GUARDRAIL_WARNINGS: Setting = Setting {
    key: "engine.tool_loop_guardrails.warnings_enabled",
    env: "ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_WARNINGS_ENABLED",
};

/// Whether `tool-guardrails` blocks calls and halts turns; off unless set to `1`, `true`, `yes`
/// or `on`.
pub const GUARDRAIL_HARD_STOP: Setting = Setting {
    key: "engine.tool_loop_guardrails.hard_stop_enabled",
    env: "ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_HARD_STOP_ENABLED",
};

/// Failures of one identical call that warn.
pub const GUARDRAIL_WARN_EXACT_FAILURE: Setting = Setting {
    key: "engine.tool_loop_guardrails.warn_after.exact_failure",
    env: "ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_WARN_AFTER_EXACT_FAILURE",
};

/// Failures of one tool that warn.
pub const GUARDRAIL_WARN_SAME_TOOL_FAILURE: Setting = Setting {
    key: "engine.tool_loop_guardrails.warn_after.same_tool_failure",
    env: "ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_WARN_AFTER_SAME_TOOL_FAILURE",
};

/// Identical results of one read-only call that warn.
pub const GUARDRAIL_WARN_NO_PROGRESS: Setting = Setting {
    key: "engine.tool_loop_guardrails.warn_after.idempotent_no_progress",
    env: "ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_WARN_AFTER_IDEMPOTENT_NO_PROGRESS",
};

/// Failures of one identical call after which hard stop blocks it.
pub const GUARDRAIL_STOP_EXACT_FAILURE: Setting = Setting {
    key: "engine.tool_loop_guardrails.hard_stop_after.exact_failure",
    env: "ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_HARD_STOP_AFTER_EXACT_FAILURE",
};

/// Failures of one tool at which hard stop halts the turn.
pub const GUARDRAIL_STOP_SAME_TOOL_FAILURE: Setting = Setting {
    key: "engine.tool_loop_guardrails.hard_stop_after.same_tool_failure",
    env: "ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_HARD_STOP_AFTER_SAME_TOOL_FAILURE",
};

/// Identical results of one read-only call after which hard stop blocks it.
pub const GUARDRAIL_STOP_NO_PROGRESS: Setting = Setting {
    key: "engine.tool_loop_guardrails.hard_stop_after.idempotent_no_progress",
    env: "ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_HARD_STOP_AFTER_IDEMPOTENT_NO_PROGRESS",
};

/// `web_search` calls one turn may make before `tool-guardrails` blocks the next; `0` is no
/// limit.
pub const GUARDRAIL_MAX_WEB_SEARCHES: Setting = Setting {
    key: "engine.tool_loop_guardrails.loop_caps.max_web_searches",
    env: "ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_LOOP_CAPS_MAX_WEB_SEARCHES",
};

/// The names of the MCP servers, comma separated; each is configured under `engine.mcp.<name>`.
pub const MCP_SERVERS: Setting = Setting {
    key: "engine.mcp_servers",
    env: "ULEI_ENGINE_MCP_SERVERS",
};

/// The `http://` URL of an MCP server on a loopback host.
pub const MCP_URL: ItemSetting = ItemSetting {
    section: "engine.mcp",
    field: "url",
};

/// The env-file whose entries are the request headers of an MCP server; unset, it sends none.
pub const MCP_ENV_FILE: ItemSetting = ItemSetting {
    section: "engine.mcp",
    field: "env_file",
};

/// Deadline in seconds of one call to an MCP server.
pub const MCP_TIMEOUT: ItemSetting = ItemSetting {
    section: "engine.mcp",
    field: "timeout",
};

/// Deadline in seconds of connecting to an MCP server and listing its tools.
pub const MCP_CONNECT_TIMEOUT: ItemSetting = ItemSetting {
    section: "engine.mcp",
    field: "connect_timeout",
};

/// The tools of an MCP server that are registered, comma separated names or globs; unset, all.
pub const MCP_TOOLS_INCLUDE: ItemSetting = ItemSetting {
    section: "engine.mcp",
    field: "tools.include",
};

/// Whether the resource tools of an MCP server are registered; on unless set otherwise.
pub const MCP_TOOLS_RESOURCES: ItemSetting = ItemSetting {
    section: "engine.mcp",
    field: "tools.resources",
};

/// Whether the prompt tools of an MCP server are registered; on unless set otherwise.
pub const MCP_TOOLS_PROMPTS: ItemSetting = ItemSetting {
    section: "engine.mcp",
    field: "tools.prompts",
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
    /// The value is not a non-negative integer.
    NotCount(Setting),
    /// The value is not a positive integer.
    NotPositive(Setting),
    /// The value is not a finite decimal number.
    NotNumber(Setting),
    /// A setting of a named item, by its key and environment variable, is invalid.
    Item {
        /// `<section>.<name>.<field>`.
        key: String,
        /// Its environment variable.
        env: String,
        /// What is wrong with it.
        fault: Fault,
    },
    /// Two items `setting` lists read the environment variable `env` alike.
    Clash {
        /// The list.
        setting: Setting,
        /// The item listed first.
        first: String,
        /// The item listed later.
        second: String,
        /// The shared variable.
        env: String,
    },
    /// The value is not an `https://` URL.
    NotHttps(Setting),
    /// The file the value names cannot be read as UTF-8 text.
    Unreadable(Setting, PathBuf, io::Error),
    /// The template the value names is malformed, for this reason.
    Template(Setting, String),
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
            Self::NotCount(s) => write!(f, "{} ({}) must be a non-negative integer", s.key, s.env),
            Self::NotPositive(s) => write!(f, "{} ({}) must be a positive integer", s.key, s.env),
            Self::NotNumber(s) => write!(f, "{} ({}) must be a decimal number", s.key, s.env),
            Self::Item { key, env, fault } => match fault {
                Fault::Missing => {
                    write!(f, "missing {key}: set it in the config file or in {env}")
                }
                Fault::Empty => write!(f, "{key} ({env}) is empty"),
                Fault::NotString => write!(f, "{key} in the config file must be a string"),
                Fault::NotSeconds => write!(f, "{key} ({env}) must be a number of seconds"),
                Fault::NotPositive => write!(f, "{key} ({env}) must be a positive integer"),
                Fault::Invalid(reason) => write!(f, "{key} ({env}) {reason}"),
            },
            Self::Clash {
                setting,
                first,
                second,
                env,
            } => write!(
                f,
                "{} ({}) names '{first}' and '{second}', which both read {env}",
                setting.key, setting.env
            ),
            Self::NotHttps(s) => write!(f, "{} ({}) must be an https:// URL", s.key, s.env),
            Self::Unreadable(s, path, e) => {
                write!(f, "{} ({}) {}: {e}", s.key, s.env, path.display())
            }
            Self::Template(s, reason) => write!(f, "{} ({}): {reason}", s.key, s.env),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(_, e) | Self::Unreadable(_, _, e) => Some(e),
            _ => None,
        }
    }
}

/// What is wrong with the value of a setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Neither the environment variable nor the config file sets it.
    Missing,
    /// It is set but empty.
    Empty,
    /// The config file sets it to a value other than a string.
    NotString,
    /// It is not a number of seconds.
    NotSeconds,
    /// It is not a positive integer.
    NotPositive,
    /// It breaks the rule this text states, as in `must be ...`.
    Invalid(&'static str),
}

impl Fault {
    /// The error of `setting` with this fault.
    fn of(self, setting: Setting) -> Error {
        match self {
            Self::Missing => Error::Missing(setting),
            Self::Empty => Error::Empty(setting),
            Self::NotString => Error::NotString(setting),
            Self::NotSeconds => Error::NotSeconds(setting),
            Self::NotPositive => Error::NotPositive(setting),
            Self::Invalid(_) => Error::Item {
                key: setting.key.to_owned(),
                env: setting.env.to_owned(),
                fault: self,
            },
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

    /// The contents of the file `setting` names as [`Config::path`] finds it, or `None` when the
    /// setting is unset.
    ///
    /// # Errors
    /// [`Error::Empty`], [`Error::NotString`] or [`Error::Unreadable`].
    pub fn file_text(
        &self,
        setting: Setting,
        env: Option<OsString>,
    ) -> Result<Option<String>, Error> {
        let path = match self.path(setting, env) {
            Ok(path) => path,
            Err(Error::Missing(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        std::fs::read_to_string(&path)
            .map(Some)
            .map_err(|e| Error::Unreadable(setting, path, e))
    }

    /// The text `setting` names: `env` when set, else the file's string value, else `None`.
    ///
    /// # Errors
    /// [`Error::Empty`] or [`Error::NotString`].
    pub fn text(&self, setting: Setting, env: Option<OsString>) -> Result<Option<String>, Error> {
        self.text_at(setting.key, env).map_err(|f| f.of(setting))
    }

    /// The text `setting` names for item `name`, as [`Config::text`] reads it.
    ///
    /// # Errors
    /// [`Error::Item`] with [`Fault::Empty`] or [`Fault::NotString`].
    pub fn item_text(
        &self,
        setting: ItemSetting,
        name: &str,
        env: &dyn Fn(&str) -> Option<OsString>,
    ) -> Result<Option<String>, Error> {
        self.text_at(&setting.key(name), env(&setting.env(name)))
            .map_err(|f| setting.error(name, f))
    }

    fn text_at(&self, key: &str, env: Option<OsString>) -> Result<Option<String>, Fault> {
        let value = match (env, self.get(key)) {
            (Some(value), _) => value.to_string_lossy().into_owned(),
            (None, Some(Value::String(value))) => value.clone(),
            (None, Some(Value::Integer(_) | Value::Bool(_))) => return Err(Fault::NotString),
            (None, None) => return Ok(None),
        };
        if value.is_empty() {
            return Err(Fault::Empty);
        }
        Ok(Some(value))
    }

    /// The `https://` URL `setting` names: `env` when set, else the file's string value.
    ///
    /// # Errors
    /// [`Error::Missing`], [`Error::Empty`], [`Error::NotString`] or [`Error::NotHttps`].
    pub fn https_url(&self, setting: Setting, env: Option<OsString>) -> Result<String, Error> {
        let url = self.text(setting, env)?.ok_or(Error::Missing(setting))?;
        if url
            .get(..8)
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"))
        {
            Ok(url)
        } else {
            Err(Error::NotHttps(setting))
        }
    }

    /// The names `setting` lists: `env` when set, else the file's string value, split at
    /// commas, each stripped, blank ones dropped; none when unset.
    ///
    /// # Errors
    /// [`Error::NotString`].
    pub fn names(&self, setting: Setting, env: Option<OsString>) -> Result<Vec<String>, Error> {
        self.names_at(setting.key, env).map_err(|f| f.of(setting))
    }

    /// The names `setting` lists for item `name`, as [`Config::names`] reads them.
    ///
    /// # Errors
    /// [`Error::Item`] with [`Fault::NotString`].
    pub fn item_names(
        &self,
        setting: ItemSetting,
        name: &str,
        env: &dyn Fn(&str) -> Option<OsString>,
    ) -> Result<Vec<String>, Error> {
        self.names_at(&setting.key(name), env(&setting.env(name)))
            .map_err(|f| setting.error(name, f))
    }

    fn names_at(&self, key: &str, env: Option<OsString>) -> Result<Vec<String>, Fault> {
        let text = match (env, self.get(key)) {
            (Some(value), _) => value.to_string_lossy().into_owned(),
            (None, Some(Value::String(value))) => value.clone(),
            (None, Some(Value::Integer(_) | Value::Bool(_))) => return Err(Fault::NotString),
            (None, None) => return Ok(Vec::new()),
        };
        Ok(text
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect())
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
        self.timeout_at(setting.key, env, default)
            .map_err(|f| f.of(setting))
    }

    /// The deadline `setting` names for item `name`, as [`Config::timeout`] reads it.
    ///
    /// # Errors
    /// [`Error::Item`] with [`Fault::NotSeconds`].
    pub fn item_timeout(
        &self,
        setting: ItemSetting,
        name: &str,
        env: &dyn Fn(&str) -> Option<OsString>,
        default: Duration,
    ) -> Result<Option<Duration>, Error> {
        self.timeout_at(&setting.key(name), env(&setting.env(name)), default)
            .map_err(|f| setting.error(name, f))
    }

    fn timeout_at(
        &self,
        key: &str,
        env: Option<OsString>,
        default: Duration,
    ) -> Result<Option<Duration>, Fault> {
        let env = env.filter(|value| !value.to_string_lossy().trim().is_empty());
        let seconds = match (env, self.get(key)) {
            (Some(value), _) => seconds(&value.to_string_lossy()),
            (None, Some(Value::String(value))) => seconds(value),
            #[allow(clippy::cast_precision_loss)]
            (None, Some(Value::Integer(value))) => Some(*value as f64),
            (None, Some(Value::Bool(_))) => None,
            (None, None) => Some(default.as_secs_f64()),
        };
        let seconds = seconds.ok_or(Fault::NotSeconds)?;
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

    /// The count `setting` names: `env` when set, else the file's integer, else `default`.
    ///
    /// # Errors
    /// [`Error::NotCount`] for a value that is not a non-negative decimal integer.
    pub fn count(
        &self,
        setting: Setting,
        env: Option<OsString>,
        default: usize,
    ) -> Result<usize, Error> {
        let count = match (env, self.get(setting.key)) {
            (Some(value), _) => value.to_string_lossy().trim().parse().ok(),
            (None, Some(Value::Integer(value))) => usize::try_from(*value).ok(),
            (None, Some(Value::String(_) | Value::Bool(_))) => None,
            (None, None) => Some(default),
        };
        count.ok_or(Error::NotCount(setting))
    }

    /// The count `setting` names as [`Config::count`] reads it, above zero.
    ///
    /// # Errors
    /// [`Error::NotPositive`] for a value that is not a positive decimal integer.
    pub fn positive(
        &self,
        setting: Setting,
        env: Option<OsString>,
        default: usize,
    ) -> Result<usize, Error> {
        match self.count(setting, env, default) {
            Ok(count) if count > 0 => Ok(count),
            _ => Err(Error::NotPositive(setting)),
        }
    }

    /// The number `setting` names: `env` when set, else the file's integer or decimal text,
    /// else `default`.
    ///
    /// # Errors
    /// [`Error::NotNumber`] for a value that is not a finite decimal number.
    pub fn number(
        &self,
        setting: Setting,
        env: Option<OsString>,
        default: f64,
    ) -> Result<f64, Error> {
        let number = match (env, self.get(setting.key)) {
            (Some(value), _) => seconds(&value.to_string_lossy()),
            (None, Some(Value::String(value))) => seconds(value),
            #[allow(clippy::cast_precision_loss)]
            (None, Some(Value::Integer(value))) => Some(*value as f64),
            (None, Some(Value::Bool(_))) => None,
            (None, None) => Some(default),
        };
        number
            .filter(|n| n.is_finite())
            .ok_or(Error::NotNumber(setting))
    }

    /// The positive integer `setting` names for route `alias`: `env` of its variable name when
    /// set, else the file's integer, else `None`.
    ///
    /// # Errors
    /// [`Error::Item`] with [`Fault::NotPositive`] for a value that is not a positive decimal
    /// integer.
    pub fn route_positive(
        &self,
        setting: ItemSetting,
        alias: &str,
        env: &dyn Fn(&str) -> Option<OsString>,
    ) -> Result<Option<u64>, Error> {
        let (key, name) = (setting.key(alias), setting.env(alias));
        let value = match (env(&name), self.get(&key)) {
            (Some(value), _) => value.to_string_lossy().trim().parse().ok(),
            (None, Some(Value::Integer(value))) => u64::try_from(*value).ok(),
            (None, Some(Value::String(_) | Value::Bool(_))) => None,
            (None, None) => return Ok(None),
        };
        match value {
            Some(value) if value > 0 => Ok(Some(value)),
            _ => Err(setting.error(alias, Fault::NotPositive)),
        }
    }

    /// The switch `setting` names: `env` when set, else the file's value, else `default`. A
    /// value is on when its lowercase text is `1`, `true`, `yes` or `on`.
    pub fn switch(&self, setting: Setting, env: Option<OsString>, default: bool) -> bool {
        self.switch_at(setting.key, env, default)
    }

    /// The switch `setting` names for item `name`, as [`Config::switch`] reads it.
    pub fn item_switch(
        &self,
        setting: ItemSetting,
        name: &str,
        env: &dyn Fn(&str) -> Option<OsString>,
        default: bool,
    ) -> bool {
        self.switch_at(&setting.key(name), env(&setting.env(name)), default)
    }

    fn switch_at(&self, key: &str, env: Option<OsString>, default: bool) -> bool {
        let text = match (env, self.get(key)) {
            (Some(value), _) => value.to_string_lossy().into_owned(),
            (None, Some(Value::Bool(value))) => return *value,
            (None, Some(Value::String(value))) => value.clone(),
            (None, Some(Value::Integer(value))) => value.to_string(),
            (None, None) => return default,
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
