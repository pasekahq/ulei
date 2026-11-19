//! Settings of `ulei-tg` from `ulei.toml` (ADR-0032 section 5). Every error names its key and
//! none carries the token.

use std::fmt;
use std::fs::{self, DirBuilder, Permissions};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::toml::{self, Value, Values};
use crate::bot_api::Config;
use crate::ingress::Zone;

/// Environment variable naming the config file when no `--config` flag is given.
pub const CONFIG_ENV: &str = "ULEI_CONFIG";

const LOCALTIME: &str = "/etc/localtime";

/// Everything `ulei-tg` reads from its config file. `Debug` hides the token.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Bot API client settings: token, base URLs, timeouts.
    pub bot: Config,
    /// Telegram user ids of the owner.
    pub owner_ids: Vec<i64>,
    /// Inbox of saved attachments; exists with mode 0700 once the settings are loaded.
    pub attachments_dir: PathBuf,
    /// Quiet window of the turn coalescer.
    pub coalesce_quiet: Duration,
    /// Cap of a coalesced turn from its first message.
    pub coalesce_max: Duration,
    /// The engine's C6 socket.
    pub engine_socket: PathBuf,
    /// Env-file holding the speech-to-text key.
    pub stt_env_file: Option<PathBuf>,
    /// Zone of the dates the agent sees.
    pub zone: Zone,
}

/// Why the settings could not be loaded: one line naming the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsError(String);

impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SettingsError {}

impl Settings {
    /// Reads the config file at `path` (none: no values), checks every key and creates the
    /// attachments directory.
    ///
    /// # Errors
    /// The file is unreadable or outside the subset, a required key is missing, a value is
    /// invalid, the token file is open to group or others, or the directory cannot be made.
    pub fn load(path: Option<&Path>) -> Result<Self, SettingsError> {
        let values = match path {
            Some(path) => {
                let text = fs::read_to_string(path)
                    .map_err(|e| fail(format!("config {}: {e}", path.display())))?;
                toml::parse(&text).map_err(|(line, reason)| {
                    fail(format!("config {} line {line}: {reason}", path.display()))
                })?
            }
            None => Values::default(),
        };
        Reader(&values).settings()
    }
}

fn fail(text: String) -> SettingsError {
    SettingsError(text)
}

struct Reader<'a>(&'a Values);

impl Reader<'_> {
    fn settings(&self) -> Result<Settings, SettingsError> {
        let token = self.token()?;
        let base_url = self.url("telegram.base_url")?;
        let mut bot = Config::new(token, base_url.as_deref());
        if let Some(file_base_url) = self.url("telegram.file_base_url")? {
            bot.file_base_url = file_base_url;
        }
        bot.connect_timeout = self.seconds("telegram.connect_timeout", bot.connect_timeout)?;
        bot.read_timeout = self.seconds("telegram.read_timeout", bot.read_timeout)?;
        bot.media_read_timeout =
            self.seconds("telegram.media_read_timeout", bot.media_read_timeout)?;
        bot.poll_timeout = self.seconds("telegram.poll_timeout", bot.poll_timeout)?;
        if bot.poll_timeout.subsec_nanos() != 0 {
            return Err(fail(
                "telegram.poll_timeout must be a whole number of seconds".to_owned(),
            ));
        }
        let owner_ids = self.owner_ids()?;
        let coalesce_quiet = self.duration("telegram.coalesce_quiet", Duration::from_secs(2))?;
        let coalesce_max = self.duration("telegram.coalesce_max", Duration::from_secs(7))?;
        let engine_socket = PathBuf::from(self.required("engine.c6_socket")?);
        let stt_env_file = self.text("telegram.stt_env_file")?.map(PathBuf::from);
        let zone = self.zone()?;
        // Last, so a config that fails a check leaves nothing behind.
        let attachments_dir = self.attachments_dir()?;
        Ok(Settings {
            bot,
            owner_ids,
            attachments_dir,
            coalesce_quiet,
            coalesce_max,
            engine_socket,
            stt_env_file,
            zone,
        })
    }

    /// The non-empty string at `key`, or `None` when unset.
    fn text(&self, key: &str) -> Result<Option<String>, SettingsError> {
        match self.0.get(key) {
            None => Ok(None),
            Some(Value::String(s)) if s.is_empty() => Err(fail(format!("{key} is empty"))),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(fail(format!("{key} must be a string"))),
        }
    }

    fn required(&self, key: &str) -> Result<String, SettingsError> {
        self.text(key)?
            .ok_or_else(|| fail(format!("missing {key}")))
    }

    fn url(&self, key: &str) -> Result<Option<String>, SettingsError> {
        match self.text(key)? {
            Some(url) if !url.starts_with("http://") && !url.starts_with("https://") => {
                Err(fail(format!("{key} must be an http:// or https:// URL")))
            }
            url => Ok(url),
        }
    }

    /// A duration of at least zero seconds: an integer or a decimal string.
    fn duration(&self, key: &str, default: Duration) -> Result<Duration, SettingsError> {
        #[allow(clippy::cast_precision_loss)]
        let seconds = match self.0.get(key) {
            None => return Ok(default),
            Some(Value::Integer(n)) => Some(*n as f64),
            Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
            Some(Value::Bool(_)) => None,
        };
        seconds
            .and_then(|s| Duration::try_from_secs_f64(s).ok())
            .ok_or_else(|| fail(format!("{key} must be a number of seconds")))
    }

    /// A timeout: a duration above zero.
    fn seconds(&self, key: &str, default: Duration) -> Result<Duration, SettingsError> {
        match self.duration(key, default)? {
            d if d.is_zero() => Err(fail(format!("{key} must be above zero"))),
            d => Ok(d),
        }
    }

    /// The token from its file, which group and others must not reach.
    fn token(&self) -> Result<String, SettingsError> {
        const KEY: &str = "telegram.token_file";
        let path = PathBuf::from(self.required(KEY)?);
        let at = |e: std::io::Error| fail(format!("{KEY} {}: {e}", path.display()));
        let mode = fs::metadata(&path).map_err(at)?.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(fail(format!(
                "{KEY} {} must have mode 0600: it is {:04o}",
                path.display(),
                mode & 0o7777
            )));
        }
        let token = fs::read_to_string(&path).map_err(at)?.trim().to_owned();
        if token.is_empty() {
            return Err(fail(format!("{KEY} {} is empty", path.display())));
        }
        Ok(token)
    }

    fn owner_ids(&self) -> Result<Vec<i64>, SettingsError> {
        const KEY: &str = "telegram.owner_ids";
        let invalid = || {
            fail(format!(
                "{KEY} must be Telegram user ids separated by commas"
            ))
        };
        let ids = match self.0.get(KEY) {
            None => return Err(fail(format!("missing {KEY}"))),
            Some(Value::Integer(id)) => vec![*id],
            Some(Value::String(list)) => list
                .split(',')
                .map(|id| id.trim().parse().map_err(|_| invalid()))
                .collect::<Result<Vec<i64>, _>>()?,
            Some(Value::Bool(_)) => return Err(invalid()),
        };
        Ok(ids)
    }

    fn attachments_dir(&self) -> Result<PathBuf, SettingsError> {
        const KEY: &str = "telegram.attachments_dir";
        let dir = PathBuf::from(self.required(KEY)?);
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .and_then(|()| fs::set_permissions(&dir, Permissions::from_mode(0o700)))
            .map_err(|e| fail(format!("{KEY} {}: {e}", dir.display())))?;
        Ok(dir)
    }

    fn zone(&self) -> Result<Zone, SettingsError> {
        if let Some(name) = self.text("engine.timezone")? {
            return Zone::named(&name).map_err(|e| fail(format!("engine.timezone {name}: {e}")));
        }
        let path = Path::new(LOCALTIME);
        match path.try_exists() {
            Ok(true) => Zone::from_file(path),
            Ok(false) => return Ok(Zone::utc()),
            Err(e) => return Err(fail(format!("{LOCALTIME}: {e}"))),
        }
        .map_err(|e| fail(format!("{LOCALTIME}: {e}")))
    }
}
