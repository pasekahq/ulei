//! The night gardener, `ulei-engine night` (ADR-0078): once per run it clears the facts the
//! garden's forget queue reaches (stage b), sorts the seedbed through the extraction model into
//! the garden's edge (stage a), sorts the history of the configured archive sources session
//! by session (stage c), and judges the new facts the garden hinted against their earlier ones
//! (stage d, ADR-0096). It is an owner-designed addition, not a port.

pub mod client;
pub mod extract;
pub mod judge;
pub mod render;
pub mod spend;
pub mod stages;
pub mod state;

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use crate::config::{self, Config, Fault, Setting};

pub use stages::{GIVE_UP_AFTER, Options, Stage, Stop, Totals, run};

/// The C4G route alias of every extraction call.
pub const ROUTE: Setting = Setting {
    key: "night.route",
    env: "ULEI_NIGHT_ROUTE",
};
/// The directory of the cursor and the run reports.
pub const STATE: Setting = Setting {
    key: "night.state",
    env: "ULEI_NIGHT_STATE",
};
/// Most USD one run spends.
pub const MAX_USD: Setting = Setting {
    key: "night.max_usd",
    env: "ULEI_NIGHT_MAX_USD",
};
/// USD per million uncached input tokens.
pub const PRICE_INPUT: Setting = Setting {
    key: "night.price_input",
    env: "ULEI_NIGHT_PRICE_INPUT",
};
/// USD per million output tokens.
pub const PRICE_OUTPUT: Setting = Setting {
    key: "night.price_output",
    env: "ULEI_NIGHT_PRICE_OUTPUT",
};
/// USD per million input tokens read from the cache.
pub const PRICE_CACHE_READ: Setting = Setting {
    key: "night.price_cache_read",
    env: "ULEI_NIGHT_PRICE_CACHE_READ",
};
/// USD per million input tokens written to the cache.
pub const PRICE_CACHE_WRITE: Setting = Setting {
    key: "night.price_cache_write",
    env: "ULEI_NIGHT_PRICE_CACHE_WRITE",
};
/// Most estimated input tokens of one call.
pub const MAX_PROMPT_TOKENS: Setting = Setting {
    key: "night.max_prompt_tokens",
    env: "ULEI_NIGHT_MAX_PROMPT_TOKENS",
};
/// Output tokens of one call.
pub const MAX_TOKENS: Setting = Setting {
    key: "night.max_tokens",
    env: "ULEI_NIGHT_MAX_TOKENS",
};
/// Seconds one call may take.
pub const TIMEOUT: Setting = Setting {
    key: "night.timeout",
    env: "ULEI_NIGHT_TIMEOUT",
};
/// Seedbed records per call.
pub const SEED_BATCH: Setting = Setting {
    key: "night.seed_batch",
    env: "ULEI_NIGHT_SEED_BATCH",
};
/// Most USD the judge of stage d spends in one run, a budget of its own.
pub const JUDGE_MAX_USD: Setting = Setting {
    key: "night.judge_max_usd",
    env: "ULEI_NIGHT_JUDGE_MAX_USD",
};
/// New facts per judge call.
pub const JUDGE_BATCH: Setting = Setting {
    key: "night.judge_batch",
    env: "ULEI_NIGHT_JUDGE_BATCH",
};
/// The archive sources of stage c.
pub const SOURCES: Setting = Setting {
    key: "night.sources",
    env: "ULEI_NIGHT_SOURCES",
};
/// The separator of a history entry's `external_id`.
pub const SEPARATOR: Setting = Setting {
    key: "night.separator",
    env: "ULEI_NIGHT_SEPARATOR",
};
/// The `who` values a transcript keeps.
pub const ROLES: Setting = Setting {
    key: "night.roles",
    env: "ULEI_NIGHT_ROLES",
};
/// Most characters of one history message.
pub const MESSAGE_CHARS: Setting = Setting {
    key: "night.message_chars",
    env: "ULEI_NIGHT_MESSAGE_CHARS",
};

/// Defaults of ADR-0078.
pub const DEFAULT_ROUTE: &str = "small";
/// Default [`MAX_USD`].
pub const DEFAULT_MAX_USD: f64 = 1.0;
/// Default [`MAX_PROMPT_TOKENS`].
pub const DEFAULT_MAX_PROMPT_TOKENS: u64 = 60_000;
/// Least and most [`MAX_PROMPT_TOKENS`].
pub const PROMPT_TOKENS_RANGE: (u64, u64) = (1_000, 100_000);
/// Default [`MAX_TOKENS`].
pub const DEFAULT_MAX_TOKENS: u64 = 8_192;
/// Default [`TIMEOUT`].
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);
/// Default [`SEED_BATCH`].
pub const DEFAULT_SEED_BATCH: usize = 20;
/// Most records of one seedbed page and so of one batch.
pub const MAX_SEED_BATCH: usize = 100;
/// Default [`JUDGE_MAX_USD`] (ADR-0096 section 3).
pub const DEFAULT_JUDGE_MAX_USD: f64 = 0.5;
/// Default [`JUDGE_BATCH`].
pub const DEFAULT_JUDGE_BATCH: usize = 25;
/// Most new facts of one judge call, and of one `reconcile_take` page.
pub const MAX_JUDGE_BATCH: usize = 100;
/// Default [`SEPARATOR`].
pub const DEFAULT_SEPARATOR: &str = "/";
/// Default [`ROLES`].
pub const DEFAULT_ROLES: [&str; 2] = ["user", "assistant"];
/// Default [`MESSAGE_CHARS`].
pub const DEFAULT_MESSAGE_CHARS: usize = 3_000;
/// Least and most [`MESSAGE_CHARS`].
pub const MESSAGE_CHARS_RANGE: (usize, usize) = (200, 16_384);

/// The settings of a run.
#[derive(Debug, Clone)]
pub struct Settings {
    /// The C4G socket of the gateway (`qb.socket`).
    pub gateway: Option<PathBuf>,
    /// The C7 socket of the garden (`garden.socket`).
    pub garden: Option<PathBuf>,
    /// The C10 socket of the archive (`memory.socket`).
    pub archive: Option<PathBuf>,
    /// The state directory.
    pub state: Option<PathBuf>,
    /// The route alias of every call.
    pub route: String,
    /// The spend limit of a run in USD.
    pub max_usd: f64,
    /// The prices of the route.
    pub prices: spend::Prices,
    /// Most estimated input tokens of one call.
    pub max_prompt_tokens: u64,
    /// Output tokens of one call.
    pub max_tokens: u64,
    /// Longest wait for one call.
    pub timeout: Duration,
    /// Seedbed records per call.
    pub seed_batch: usize,
    /// The judge's own spend limit of a run in USD.
    pub judge_max_usd: f64,
    /// New facts per judge call.
    pub judge_batch: usize,
    /// The archive sources of stage c, in order.
    pub sources: Vec<String>,
    /// The separator of an entry's `external_id`.
    pub separator: String,
    /// The `who` values a transcript keeps.
    pub roles: Vec<String>,
    /// Most characters of one history message.
    pub message_chars: usize,
}

impl Settings {
    /// The settings of `config`, each `ULEI_NIGHT_*` variable read through `env` winning over its
    /// key. A socket or the state directory left unset is `None`; a run that needs it refuses to
    /// start.
    ///
    /// # Errors
    /// [`config::Error`] of the first setting that is set but invalid.
    pub fn read(
        config: &Config,
        env: &dyn Fn(&str) -> Option<OsString>,
    ) -> Result<Self, config::Error> {
        let optional_path = |setting: Setting| match config.path(setting, env(setting.env)) {
            Ok(path) => Ok(Some(path)),
            Err(config::Error::Missing(_)) => Ok(None),
            Err(e) => Err(e),
        };
        let invalid = |setting: Setting, rule| config::Error::Item {
            key: setting.key.to_owned(),
            env: setting.env.to_owned(),
            fault: Fault::Invalid(rule),
        };
        let route = config
            .text(ROUTE, env(ROUTE.env))?
            .unwrap_or_else(|| DEFAULT_ROUTE.to_owned());
        if !crate::serve::is_route_alias(&route) {
            return Err(invalid(ROUTE, "must be a qb route alias"));
        }
        let usd = |setting: Setting, default: f64| {
            let value = config.number(setting, env(setting.env), default)?;
            if value < 0.0 {
                return Err(invalid(setting, "must be 0 or more"));
            }
            Ok(value)
        };
        let max_usd = usd(MAX_USD, DEFAULT_MAX_USD)?;
        let prices = spend::Prices {
            input: usd(PRICE_INPUT, spend::Prices::DEFAULT.input)?,
            output: usd(PRICE_OUTPUT, spend::Prices::DEFAULT.output)?,
            cache_read: usd(PRICE_CACHE_READ, spend::Prices::DEFAULT.cache_read)?,
            cache_write: usd(PRICE_CACHE_WRITE, spend::Prices::DEFAULT.cache_write)?,
        };
        let max_prompt_tokens = wide(config.positive(
            MAX_PROMPT_TOKENS,
            env(MAX_PROMPT_TOKENS.env),
            narrow(DEFAULT_MAX_PROMPT_TOKENS),
        )?);
        let (low, high) = PROMPT_TOKENS_RANGE;
        if !(low..=high).contains(&max_prompt_tokens) {
            return Err(invalid(MAX_PROMPT_TOKENS, "must be 1000 to 100000"));
        }
        let max_tokens =
            wide(config.positive(MAX_TOKENS, env(MAX_TOKENS.env), narrow(DEFAULT_MAX_TOKENS))?);
        let timeout = config
            .timeout(TIMEOUT, env(TIMEOUT.env), DEFAULT_TIMEOUT)?
            .unwrap_or(config::MAX_TIMEOUT);
        let seed_batch = config.positive(SEED_BATCH, env(SEED_BATCH.env), DEFAULT_SEED_BATCH)?;
        if seed_batch > MAX_SEED_BATCH {
            return Err(invalid(SEED_BATCH, "must be 1 to 100"));
        }
        let judge_max_usd = usd(JUDGE_MAX_USD, DEFAULT_JUDGE_MAX_USD)?;
        let judge_batch =
            config.positive(JUDGE_BATCH, env(JUDGE_BATCH.env), DEFAULT_JUDGE_BATCH)?;
        if judge_batch > MAX_JUDGE_BATCH {
            return Err(invalid(JUDGE_BATCH, "must be 1 to 100"));
        }
        let sources = config.names(SOURCES, env(SOURCES.env));
        if sources.iter().any(|s| !is_source(s)) {
            return Err(invalid(SOURCES, "must list archive source names"));
        }
        let separator = config
            .text(SEPARATOR, env(SEPARATOR.env))?
            .unwrap_or_else(|| DEFAULT_SEPARATOR.to_owned());
        let mut roles = config.names(ROLES, env(ROLES.env));
        if roles.is_empty() {
            roles = DEFAULT_ROLES.iter().map(|&r| r.to_owned()).collect();
        }
        let message_chars =
            config.positive(MESSAGE_CHARS, env(MESSAGE_CHARS.env), DEFAULT_MESSAGE_CHARS)?;
        let (low, high) = MESSAGE_CHARS_RANGE;
        if !(low..=high).contains(&message_chars) {
            return Err(invalid(MESSAGE_CHARS, "must be 200 to 16384"));
        }
        Ok(Self {
            gateway: optional_path(config::QB_SOCKET)?,
            garden: optional_path(config::GARDEN_SOCKET)?,
            archive: optional_path(config::MEMORY_SOCKET)?,
            state: optional_path(STATE)?,
            route,
            max_usd,
            prices,
            max_prompt_tokens,
            max_tokens,
            timeout,
            seed_batch,
            judge_max_usd,
            judge_batch,
            sources,
            separator,
            roles,
            message_chars,
        })
    }
}

fn wide(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

fn narrow(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

/// Whether `name` is a C10 `Source`: `^[a-z][a-z0-9_-]{0,63}$`.
fn is_source(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_lowercase())
        && name.len() <= 64
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::is_source;

    #[test]
    fn source_names_follow_c10() {
        assert!(is_source("dialogue"));
        assert!(is_source("old-runtime_2"));
        assert!(!is_source("Old"));
        assert!(!is_source("1a"));
        assert!(!is_source(""));
        assert!(!is_source(&"a".repeat(65)));
    }
}
