//! Settings of `ulei-tg` from `ulei.toml`, decoded by `ulei_contracts::config` (ADR-0032
//! section 5). Every error names its key and none carries the token.

use std::fmt;
use std::fs::{self, DirBuilder, Permissions};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use ulei_contracts::config::{Config as File, Value};

use super::guest_gate::{GATE_TIMEOUT, GuestGateConfig};
use crate::bot_api::Config;
use crate::ingress::{ReactionConfig, ZONEINFO_DIRS, Zone};
use crate::stt::SttConfig;

const LOCALTIME: &str = "/etc/localtime";
/// Default bound of a connect to the engine's C6 socket.
const ENGINE_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Default bound of a whole C6 call; above the engine's own 5 s request timeout.
const ENGINE_CALL_TIMEOUT: Duration = Duration::from_secs(10);

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
    /// Bound of a connect to that socket.
    pub engine_connect_timeout: Duration,
    /// Bound of a whole C6 call, connect included.
    pub engine_call_timeout: Duration,
    /// Speech to text of voice notes; none without `telegram.stt_env_file`.
    pub stt: Option<SttConfig>,
    /// Echo each voice transcript to the owner before its turn (`telegram.stt_echo_transcripts`).
    pub stt_echo_transcripts: bool,
    /// Zone of the dates the agent sees.
    pub zone: Zone,
    /// Wording and delay of the reaction wake-up.
    pub reactions: ReactionConfig,
    /// The guest gate that gets the owner's protocol updates first (ADR-0083); none without
    /// `telegram.guest_gate_url`.
    pub guest_gate: Option<GuestGateConfig>,
}

/// The keys a direct notice needs (ADR-0097): the Bot API client and the owner ids, without the
/// daemon's engine socket and attachments directory. `Debug` hides the token.
#[derive(Debug, Clone)]
pub struct Transport {
    /// Bot API client settings: token, base URLs, timeouts.
    pub bot: Config,
    /// Telegram user ids of the owner.
    pub owner_ids: Vec<i64>,
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
    /// Decodes the config file `flag` names, else the one `ulei_contracts::config` finds in the
    /// process environment, checks the keys `ulei-tg` reads and creates the attachments directory.
    ///
    /// # Errors
    /// No file is found, it is unreadable or refused by the decoder, a required key is missing,
    /// a value is invalid, the token file is open to group or others, or the directory cannot
    /// be made.
    pub fn load(flag: Option<&Path>) -> Result<Self, SettingsError> {
        let file = File::read(flag).map_err(|e| fail(e.to_string()))?;
        Reader(&file).settings()
    }
}

impl Transport {
    /// Decodes the config file as [`Settings::load`] does and reads only the `telegram` keys of
    /// the Bot API client and `telegram.owner_ids`; it creates nothing.
    ///
    /// # Errors
    /// No file is found, it is unreadable or refused by the decoder, the token file or the owner
    /// ids are missing or invalid, or a client key is invalid.
    pub fn load(flag: Option<&Path>) -> Result<Self, SettingsError> {
        let file = File::read(flag).map_err(|e| fail(e.to_string()))?;
        let reader = Reader(&file);
        Ok(Self {
            bot: reader.bot()?,
            owner_ids: reader.owner_ids()?,
        })
    }
}

fn fail(text: String) -> SettingsError {
    SettingsError(text)
}

struct Reader<'a>(&'a File);

/// The trimmed content of the secret file `path` of `key`, which group and others must not reach.
fn secret(key: &str, path: &Path) -> Result<String, SettingsError> {
    let at = |e: std::io::Error| fail(format!("{key} {}: {e}", path.display()));
    let mode = fs::metadata(path).map_err(at)?.permissions().mode();
    if mode & 0o077 != 0 {
        return Err(fail(format!(
            "{key} {} must have mode 0600: it is {:04o}",
            path.display(),
            mode & 0o7777
        )));
    }
    let token = fs::read_to_string(path).map_err(at)?.trim().to_owned();
    if token.is_empty() {
        return Err(fail(format!("{key} {} is empty", path.display())));
    }
    Ok(token)
}

/// Whether `url` may carry the guest gate's token: plain HTTP only to a loopback host, HTTPS to
/// any host, never user info in the authority.
fn gate_url_ok(url: &str) -> bool {
    let (rest, loopback_only) = if let Some(rest) = url.strip_prefix("http://") {
        (rest, true)
    } else if let Some(rest) = url.strip_prefix("https://") {
        (rest, false)
    } else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') || url.chars().any(char::is_whitespace) {
        return false;
    }
    if !loopback_only {
        return true;
    }
    let host = if authority.starts_with('[') {
        authority.split_inclusive(']').next().unwrap_or_default()
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    matches!(host, "127.0.0.1" | "[::1]" | "localhost")
}

impl Reader<'_> {
    fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key).map(|entry| &entry.value)
    }

    fn settings(&self) -> Result<Settings, SettingsError> {
        let bot = self.bot()?;
        let owner_ids = self.owner_ids()?;
        let coalesce_quiet = self.duration("telegram.coalesce_quiet", Duration::from_secs(2))?;
        let coalesce_max = self.duration("telegram.coalesce_max", Duration::from_secs(7))?;
        let engine_socket = PathBuf::from(self.required("engine.c6_socket")?);
        let engine_connect_timeout =
            self.seconds("telegram.engine_connect_timeout", ENGINE_CONNECT_TIMEOUT)?;
        let engine_call_timeout =
            self.seconds("telegram.engine_call_timeout", ENGINE_CALL_TIMEOUT)?;
        let stt = self.stt()?;
        let stt_echo_transcripts = self.flag("telegram.stt_echo_transcripts", true);
        let zone = self.zone()?;
        let reactions = self.reactions()?;
        let guest_gate = self.guest_gate()?;
        // Last, so a config that fails a check leaves nothing behind.
        let attachments_dir = self.attachments_dir()?;
        Ok(Settings {
            bot,
            owner_ids,
            attachments_dir,
            coalesce_quiet,
            coalesce_max,
            engine_socket,
            engine_connect_timeout,
            engine_call_timeout,
            stt,
            stt_echo_transcripts,
            zone,
            reactions,
            guest_gate,
        })
    }

    /// The Bot API client settings: the token from its file, the base URLs and the timeouts.
    fn bot(&self) -> Result<Config, SettingsError> {
        let token = self.token()?;
        let base_url = self.url("telegram.base_url")?;
        let mut bot = Config::new(token, base_url.as_deref());
        if let Some(file_base_url) = self.url("telegram.file_base_url")? {
            bot.file_base_url = file_base_url;
        }
        bot.connect_timeout = self.seconds("telegram.connect_timeout", bot.connect_timeout)?;
        bot.read_timeout = self.seconds("telegram.read_timeout", bot.read_timeout)?;
        bot.write_timeout = self.seconds("telegram.write_timeout", bot.write_timeout)?;
        bot.media_read_timeout =
            self.seconds("telegram.media_read_timeout", bot.media_read_timeout)?;
        bot.poll_timeout = self.seconds("telegram.poll_timeout", bot.poll_timeout)?;
        if bot.poll_timeout.subsec_nanos() != 0 {
            return Err(fail(
                "telegram.poll_timeout must be a whole number of seconds".to_owned(),
            ));
        }
        Ok(bot)
    }

    /// `telegram.guest_gate_url` and `telegram.guest_gate_token_file`, both or neither. An error
    /// names the key and the rule, never the URL or the token.
    fn guest_gate(&self) -> Result<Option<GuestGateConfig>, SettingsError> {
        const URL: &str = "telegram.guest_gate_url";
        const FILE: &str = "telegram.guest_gate_token_file";
        let (url, file) = match (self.text(URL)?, self.text(FILE)?) {
            (None, None) => return Ok(None),
            (Some(_), None) => return Err(fail(format!("{URL} needs {FILE}"))),
            (None, Some(_)) => return Err(fail(format!("{FILE} needs {URL}"))),
            (Some(url), Some(file)) => (url, file),
        };
        if !gate_url_ok(&url) {
            return Err(fail(format!(
                "{URL} must be an http:// URL on 127.0.0.1, [::1] or localhost, or an https:// URL, without user info"
            )));
        }
        let token = secret(FILE, Path::new(&file))?;
        if !token.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(fail(format!(
                "{FILE} {file} must hold one token of visible ASCII characters"
            )));
        }
        Ok(Some(GuestGateConfig {
            url,
            token,
            timeout: GATE_TIMEOUT,
        }))
    }

    /// The endpoint keys are checked even without the env-file that turns the client on.
    fn stt(&self) -> Result<Option<SttConfig>, SettingsError> {
        let base_url = self.url("telegram.stt_base_url")?;
        let model = self.text("telegram.stt_model")?;
        Ok(self.text("telegram.stt_env_file")?.map(|env_file| {
            let mut config = SttConfig::new(PathBuf::from(env_file));
            if let Some(base_url) = base_url {
                config.base_url = base_url;
            }
            if let Some(model) = model {
                config.model = model;
            }
            config
        }))
    }

    fn reactions(&self) -> Result<ReactionConfig, SettingsError> {
        let default = ReactionConfig::default();
        Ok(ReactionConfig {
            wake_delay: self.duration("telegram.reaction_wake_delay", default.wake_delay)?,
            wake_text: self
                .text("telegram.reaction_wake_text")?
                .unwrap_or(default.wake_text),
            context_header: self
                .text("telegram.reaction_context_header")?
                .unwrap_or(default.context_header),
        })
    }

    /// The non-empty string at the string key `key`, or `None` when unset.
    fn text(&self, key: &str) -> Result<Option<String>, SettingsError> {
        match self.get(key) {
            Some(Value::String(s)) if s.is_empty() => Err(fail(format!("{key} is empty"))),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            _ => Ok(None),
        }
    }

    /// The boolean at the boolean key `key`, else `default`.
    fn flag(&self, key: &str, default: bool) -> bool {
        match self.get(key) {
            Some(Value::Bool(b)) => *b,
            _ => default,
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
        let seconds = match self.get(key) {
            Some(Value::Integer(n)) => Some(*n as f64),
            Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
            _ => return Ok(default),
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
        secret(KEY, &path)
    }

    fn owner_ids(&self) -> Result<Vec<i64>, SettingsError> {
        const KEY: &str = "telegram.owner_ids";
        let invalid = || {
            fail(format!(
                "{KEY} must be Telegram user ids separated by commas"
            ))
        };
        let ids = match self.get(KEY) {
            Some(Value::Integer(id)) => vec![*id],
            Some(Value::String(list)) => list
                .split(',')
                .map(|id| id.trim().parse().map_err(|_| invalid()))
                .collect::<Result<Vec<i64>, _>>()?,
            _ => return Err(fail(format!("missing {KEY}"))),
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
            let dirs = ZONEINFO_DIRS.map(Path::new);
            return Zone::named(&name, &dirs)
                .map_err(|e| fail(format!("engine.timezone {name}: {e}")));
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

#[cfg(test)]
mod tests {
    use super::gate_url_ok;

    #[test]
    fn gate_urls_keep_the_token_on_loopback_or_tls() {
        for good in [
            "http://127.0.0.1:8792/internal/telegram",
            "http://localhost:8792/internal/telegram",
            "http://[::1]:8792/internal/telegram",
            "http://127.0.0.1/internal/telegram",
            "https://gate.example.invalid/internal/telegram",
        ] {
            assert!(gate_url_ok(good), "{good}");
        }
        for bad in [
            "http://192.0.2.1:8792/internal/telegram",
            "http://127.0.0.1.example.invalid/x",
            "http://localhost.example.invalid/x",
            "http://user:pw@127.0.0.1:8792/x",
            "https://user@gate.example.invalid/x",
            "ftp://127.0.0.1/x",
            "127.0.0.1:8792",
            "http://",
            "http://127.0.0.1:8792/x y",
        ] {
            assert!(!gate_url_ok(bad), "{bad}");
        }
    }
}
