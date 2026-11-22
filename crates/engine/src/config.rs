// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Settings of `ulei-engine run` and `serve`: each comes from its environment variable, else
//! from `ulei.toml` as `ulei_contracts::config` decodes it, else from its default.

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ulei_contracts::config::{self as file, Value};
use ulei_contracts::generated::c4g::CacheMode;

use crate::policy::MalformedGrep;

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

/// Unix socket of the garden's C7, which `forget` calls; unset, the garden is not configured.
pub const GARDEN_SOCKET: Setting = Setting {
    key: "garden.socket",
    env: "ULEI_GARDEN_SOCKET",
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

/// The prompt cache mode of the turn, compaction and `no-ask-guard` classifier requests:
/// `auto_1h`, `auto`, or `off` for none.
pub const PROMPT_CACHE: Setting = Setting {
    key: "engine.prompt_cache",
    env: "ULEI_ENGINE_PROMPT_CACHE",
};

/// What the hardline floor does with a command whose grep pattern operands do not parse: `warn`
/// or `block`.
pub const HARDLINE_MALFORMED_MODE: Setting = Setting {
    key: "engine.hardline_malformed_mode",
    env: "ULEI_ENGINE_HARDLINE_MALFORMED_MODE",
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

/// The skills directory: `skills_list` and `skill_view` read it, the skills index lists it, and
/// C6 `skills_dir` names it.
pub const SKILLS_DIR: Setting = Setting {
    key: "engine.skills_dir",
    env: "ULEI_ENGINE_SKILLS_DIR",
};

/// Comma-separated names of the skills the index, `skills_list` and `skill_view` leave out.
pub const DISABLED_SKILLS: Setting = Setting {
    key: "engine.disabled_skills",
    env: "ULEI_ENGINE_DISABLED_SKILLS",
};

/// Whether the system prompt carries the skills index; off unless set to `1`, `true`, `yes` or
/// `on`.
pub const SKILLS_INDEX: Setting = Setting {
    key: "engine.skills_index",
    env: "ULEI_ENGINE_SKILLS_INDEX",
};

/// The file telling how to reach the task board, which the task-delegation meta-skill names and
/// the engine does not read.
pub const TASK_BOARD_FILE: Setting = Setting {
    key: "engine.task_board_file",
    env: "ULEI_ENGINE_TASK_BOARD_FILE",
};

/// The file whose stripped text replaces the shipped text of a core item.
pub const CORE_FILE: ItemSetting = ItemSetting {
    section: "engine.core",
    field: "file",
};

/// Whether a core item is in the prompt; on unless set to another value than `1`, `true`, `yes`
/// or `on`.
pub const CORE_ENABLED: ItemSetting = ItemSetting {
    section: "engine.core",
    field: "enabled",
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

/// Base URL of the Exa API, an `https://` URL or, under the `web-loopback-http` test switch, an
/// `http://` one to a loopback address literal; required with [`WEB_HELPER`].
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

/// Why the settings could not be resolved.
#[derive(Debug)]
pub enum Error {
    /// The config file could not be found, read or decoded.
    File(file::Error),
    /// A file section `[engine.mcp.<name>]` holds a server that [`MCP_SERVERS`] does not list.
    Unlisted {
        /// The config file.
        path: PathBuf,
        /// Line number of the section header, from 1.
        line: usize,
        /// The server name.
        name: String,
    },
    /// Neither the environment variable nor the config file sets the value.
    Missing(Setting),
    /// A key or section header names an item that the section does not know.
    UnknownItem {
        /// The config file.
        path: PathBuf,
        /// Line number of the key or header, from 1.
        line: usize,
        /// The dotted key or section name.
        name: String,
        /// The items the section knows.
        known: Vec<&'static str>,
    },
    /// The value is set but empty.
    Empty(Setting),
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
    /// The file a setting of a named item names cannot be read as UTF-8 text.
    ItemUnreadable {
        /// `<section>.<name>.<field>`.
        key: String,
        /// Its environment variable.
        env: String,
        /// The file.
        path: PathBuf,
        /// Why it cannot be read.
        error: io::Error,
    },
    /// The template the value names is malformed, for this reason.
    Template(Setting, String),
    /// A route alias contains `.`, so its settings would sit in nested sections.
    RouteAlias(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(e) => e.fmt(f),
            Self::Unlisted { path, line, name } => write!(
                f,
                "config {} line {line}: section [{}.{name}] is not listed in {} ({})",
                path.display(),
                MCP_URL.section,
                MCP_SERVERS.key,
                MCP_SERVERS.env
            ),
            Self::Missing(s) => write!(
                f,
                "missing {}: set it in the config file or in {}",
                s.key, s.env
            ),
            Self::UnknownItem {
                path,
                line,
                name,
                known,
            } => write!(
                f,
                "config {} line {line}: {name} names an unknown id; known: {}",
                path.display(),
                known.join(", ")
            ),
            Self::Empty(s) => write!(f, "{} ({}) is empty", s.key, s.env),
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
            Self::ItemUnreadable {
                key,
                env,
                path,
                error,
            } => write!(f, "{key} ({env}) {}: {error}", path.display()),
            Self::Template(s, reason) => write!(f, "{} ({}): {reason}", s.key, s.env),
            Self::RouteAlias(alias) => write!(
                f,
                "route alias '{alias}' contains '.', which the config file cannot name in engine.routes"
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::File(e) => Some(e),
            Self::Unreadable(_, _, e) | Self::ItemUnreadable { error: e, .. } => Some(e),
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

/// The decoded config file. The decoder has checked every key against C9, so each value has the
/// kind the catalogue gives its key.
#[derive(Debug, Clone)]
pub struct Config(file::Config);

impl Config {
    /// The config file `flag` names, else the one [`file::Config::read`] finds in the process
    /// environment, decoded; `env` reads the setting variables.
    ///
    /// # Errors
    /// [`Error::File`], or [`Error::Unlisted`] for the first header of a server section that
    /// [`MCP_SERVERS`] does not list, keys under it or not.
    pub fn read(
        flag: Option<&Path>,
        env: &dyn Fn(&str) -> Option<OsString>,
    ) -> Result<Self, Error> {
        let config = Self(file::Config::read(flag).map_err(Error::File)?);
        let listed = config.names(MCP_SERVERS, env(MCP_SERVERS.env));
        let prefix = format!("{}.", MCP_URL.section);
        let unlisted = config.0.sections.iter().find_map(|section| {
            let rest = section.name.strip_prefix(&prefix)?;
            let name = rest.split_once('.').map_or(rest, |(name, _)| name);
            (!listed.iter().any(|n| n == name)).then(|| (section.line, name.to_owned()))
        });
        match unlisted {
            Some((line, name)) => Err(Error::Unlisted {
                path: config.0.path.clone(),
                line,
                name,
            }),
            None => Ok(config),
        }
    }

    /// Checks that every key and section header of the file under `section` names an item of
    /// `known`.
    ///
    /// # Errors
    /// [`Error::UnknownItem`] for the first that does not, in file order.
    pub fn known_items(&self, section: &str, known: &[&'static str]) -> Result<(), Error> {
        let prefix = format!("{section}.");
        let unknown = |name: &str| {
            name.strip_prefix(&prefix)
                .and_then(|rest| rest.split('.').next())
                .is_some_and(|item| !known.contains(&item))
        };
        let keys = self.0.entries.iter().map(|e| (e.line, &e.key));
        let headers = self.0.sections.iter().map(|s| (s.line, &s.name));
        let first = keys
            .chain(headers)
            .filter(|(_, name)| unknown(name))
            .min_by_key(|(line, _)| *line);
        match first {
            Some((line, name)) => Err(Error::UnknownItem {
                path: self.0.path.clone(),
                line,
                name: name.clone(),
                known: known.to_vec(),
            }),
            None => Ok(()),
        }
    }

    fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key).map(|entry| &entry.value)
    }

    /// `env` as text when set, else the file's text at the string key `key`.
    fn string(&self, key: &str, env: Option<OsString>) -> Option<String> {
        match (env, self.get(key)) {
            (Some(value), _) => Some(value.to_string_lossy().into_owned()),
            (None, Some(Value::String(value))) => Some(value.clone()),
            (None, _) => None,
        }
    }

    /// The path `setting` names: `env` when set, else the file's string value.
    ///
    /// # Errors
    /// [`Error::Missing`] or [`Error::Empty`].
    pub fn path(&self, setting: Setting, env: Option<OsString>) -> Result<PathBuf, Error> {
        let value = match env {
            Some(value) => value,
            None => self
                .string(setting.key, None)
                .ok_or(Error::Missing(setting))?
                .into(),
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
    /// [`Error::Empty`] or [`Error::Unreadable`].
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
    /// [`Error::Empty`].
    pub fn text(&self, setting: Setting, env: Option<OsString>) -> Result<Option<String>, Error> {
        self.text_at(setting.key, env).map_err(|f| f.of(setting))
    }

    /// The text `setting` names for item `name`, as [`Config::text`] reads it.
    ///
    /// # Errors
    /// [`Error::Item`] with [`Fault::Empty`].
    pub fn item_text(
        &self,
        setting: ItemSetting,
        name: &str,
        env: &dyn Fn(&str) -> Option<OsString>,
    ) -> Result<Option<String>, Error> {
        self.text_at(&setting.key(name), env(&setting.env(name)))
            .map_err(|f| setting.error(name, f))
    }

    /// The contents of the file `setting` names for item `name`, or `None` when it is unset.
    ///
    /// # Errors
    /// [`Error::Item`] with [`Fault::Empty`], or [`Error::ItemUnreadable`].
    pub fn item_file_text(
        &self,
        setting: ItemSetting,
        name: &str,
        env: &dyn Fn(&str) -> Option<OsString>,
    ) -> Result<Option<String>, Error> {
        let Some(path) = self.item_text(setting, name, env)?.map(PathBuf::from) else {
            return Ok(None);
        };
        std::fs::read_to_string(&path)
            .map(Some)
            .map_err(|error| Error::ItemUnreadable {
                key: setting.key(name),
                env: setting.env(name),
                path,
                error,
            })
    }

    fn text_at(&self, key: &str, env: Option<OsString>) -> Result<Option<String>, Fault> {
        match self.string(key, env) {
            Some(value) if value.is_empty() => Err(Fault::Empty),
            value => Ok(value),
        }
    }

    /// The prompt cache mode `setting` names: `env` when set, else the file's string value, else
    /// `auto_1h`; `off` is `None`.
    ///
    /// # Errors
    /// [`Error::Empty`], or [`Error::Item`] with [`Fault::Invalid`] for any other value.
    pub fn cache_mode(
        &self,
        setting: Setting,
        env: Option<OsString>,
    ) -> Result<Option<CacheMode>, Error> {
        match self.text(setting, env)?.as_deref() {
            None | Some("auto_1h") => Ok(Some(CacheMode::Auto1h)),
            Some("auto") => Ok(Some(CacheMode::Auto)),
            Some("off") => Ok(None),
            Some(_) => Err(Fault::Invalid("must be auto_1h, auto or off").of(setting)),
        }
    }

    /// The malformed-grep handling of the hardline floor `setting` names: `env` when set, else
    /// the file's string value, else `warn`.
    ///
    /// # Errors
    /// [`Error::Empty`], or [`Error::Item`] with [`Fault::Invalid`] for any other value.
    pub fn malformed_grep(
        &self,
        setting: Setting,
        env: Option<OsString>,
    ) -> Result<MalformedGrep, Error> {
        match self.text(setting, env)?.as_deref() {
            None | Some("warn") => Ok(MalformedGrep::Warn),
            Some("block") => Ok(MalformedGrep::Block),
            Some(_) => Err(Fault::Invalid("must be warn or block").of(setting)),
        }
    }

    /// The `https://` URL `setting` names: `env` when set, else the file's string value. Under
    /// the `web-loopback-http` test switch an `http://` URL to a loopback address literal too.
    ///
    /// # Errors
    /// [`Error::Missing`], [`Error::Empty`] or [`Error::NotHttps`].
    pub fn https_url(&self, setting: Setting, env: Option<OsString>) -> Result<String, Error> {
        let url = self.text(setting, env)?.ok_or(Error::Missing(setting))?;
        let https = url
            .get(..8)
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"));
        #[cfg(feature = "web-loopback-http")]
        let https = https || crate::web::is_loopback_http_url(&url);
        if https {
            Ok(url)
        } else {
            Err(Error::NotHttps(setting))
        }
    }

    /// The names `setting` lists: `env` when set, else the file's string value, split at
    /// commas, each stripped, blank ones dropped; none when unset.
    pub fn names(&self, setting: Setting, env: Option<OsString>) -> Vec<String> {
        self.names_at(setting.key, env)
    }

    /// The names `setting` lists for item `name`, as [`Config::names`] reads them.
    pub fn item_names(
        &self,
        setting: ItemSetting,
        name: &str,
        env: &dyn Fn(&str) -> Option<OsString>,
    ) -> Vec<String> {
        self.names_at(&setting.key(name), env(&setting.env(name)))
    }

    fn names_at(&self, key: &str, env: Option<OsString>) -> Vec<String> {
        self.string(key, env)
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect()
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
        let seconds = self
            .decimal(key, env, default.as_secs_f64())
            .ok_or(Fault::NotSeconds)?;
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
            (None, _) => return Ok(default),
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
            (None, _) => Some(default),
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
        self.decimal(setting.key, env, default)
            .filter(|n| n.is_finite())
            .ok_or(Error::NotNumber(setting))
    }

    /// `env` when set, else the file's integer or decimal text at the key `key`, else
    /// `default`, as a number; `None` when the value is no number.
    fn decimal(&self, key: &str, env: Option<OsString>, default: f64) -> Option<f64> {
        match (env, self.get(key)) {
            (Some(value), _) => seconds(&value.to_string_lossy()),
            (None, Some(Value::String(value))) => seconds(value),
            #[allow(clippy::cast_precision_loss)]
            (None, Some(Value::Integer(value))) => Some(*value as f64),
            (None, _) => Some(default),
        }
    }

    /// The positive integer `setting` names for route `alias`: `env` of its variable name when
    /// set, else the file's integer, else `None`.
    ///
    /// # Errors
    /// [`Error::RouteAlias`] for an alias with a `.`, [`Error::Item`] with [`Fault::NotPositive`]
    /// for a value that is not a positive decimal integer.
    pub fn route_positive(
        &self,
        setting: ItemSetting,
        alias: &str,
        env: &dyn Fn(&str) -> Option<OsString>,
    ) -> Result<Option<u64>, Error> {
        // A dotted alias would be read as nested sections, so its settings would silently miss.
        if alias.contains('.') {
            return Err(Error::RouteAlias(alias.to_owned()));
        }
        let (key, name) = (setting.key(alias), setting.env(alias));
        let value = match (env(&name), self.get(&key)) {
            (Some(value), _) => value.to_string_lossy().trim().parse().ok(),
            (None, Some(Value::Integer(value))) => u64::try_from(*value).ok(),
            (None, _) => return Ok(None),
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
