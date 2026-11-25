//! The settings of `[hive_mcp]` in `ulei.toml` (C9), read through `ulei_contracts::config`, and
//! the bearer token of the file `token_file` names.

use std::fs;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ulei_contracts::config::{Config, Value};

/// The actor of every call when `hive_mcp.actor` is unset.
pub const DEFAULT_ACTOR: &str = "secretary";
/// Seconds of one CLI run when `hive_mcp.timeout` is unset.
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// The fewest characters of the token.
pub const MIN_TOKEN_CHARS: usize = 32;
/// The most characters of the token.
pub const MAX_TOKEN_CHARS: usize = 4096;

/// The checked settings of the server.
#[derive(Clone, PartialEq, Eq)]
pub struct Settings {
    /// The loopback address it listens on.
    pub listen: SocketAddr,
    /// The bearer token the engine must send.
    pub token: String,
    /// The argv of the hive's CLI, its first word an absolute path.
    pub command: Vec<String>,
    /// The hive's config, handed to the CLI as `HIVE_CONFIG`.
    pub hive_config: PathBuf,
    /// The actor of every call.
    pub actor: String,
    /// The longest a CLI run may take.
    pub timeout: Duration,
    /// Every card created is held for the owner's release.
    pub hold: bool,
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("listen", &self.listen)
            .field("token", &"[REDACTED]")
            .field("command", &self.command)
            .field("hive_config", &self.hive_config)
            .field("actor", &self.actor)
            .field("timeout", &self.timeout)
            .field("hold", &self.hold)
            .finish()
    }
}

impl Settings {
    /// The settings of `config`, the token read from its file.
    ///
    /// # Errors
    /// One line naming the key at fault; never a value of the token.
    pub fn from_config(config: &Config) -> Result<Self, String> {
        let string = |key: &str| match config.get(&format!("hive_mcp.{key}")).map(|e| &e.value) {
            Some(Value::String(s)) => Ok(Some(s.clone())),
            None => Ok(None),
            Some(_) => Err(format!("hive_mcp.{key} must be a string")),
        };
        let required = |key: &str| {
            string(key)?
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| format!("hive_mcp.{key} is required"))
        };
        let listen = parse_listen(&required("listen")?)?;
        let token_file = PathBuf::from(required("token_file")?);
        let token = read_token(&token_file)?;
        let command = parse_command(&required("hive_command")?)?;
        let hive_config = PathBuf::from(required("hive_config")?);
        if !hive_config.is_absolute() {
            return Err("hive_mcp.hive_config must be an absolute path".into());
        }
        let actor = string("actor")?.unwrap_or_else(|| DEFAULT_ACTOR.to_owned());
        if !valid_actor(&actor) {
            return Err(
                "hive_mcp.actor must be 1 to 64 of A-Z a-z 0-9 . _ : @ - not starting with -"
                    .into(),
            );
        }
        let timeout = match config.get("hive_mcp.timeout").map(|e| &e.value) {
            None => DEFAULT_TIMEOUT_SECS,
            Some(Value::Integer(n)) if (1..=3600).contains(n) => n.unsigned_abs(),
            Some(_) => return Err("hive_mcp.timeout must be an integer from 1 to 3600".into()),
        };
        let hold = match config.get("hive_mcp.hold").map(|e| &e.value) {
            None => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err("hive_mcp.hold must be true or false".into()),
        };
        Ok(Self {
            listen,
            token,
            command,
            hive_config,
            actor,
            timeout: Duration::from_secs(timeout),
            hold,
        })
    }
}

/// The address of `text`: an IP address on loopback with a port.
///
/// # Errors
/// The rule it breaks.
pub fn parse_listen(text: &str) -> Result<SocketAddr, String> {
    let rule = "hive_mcp.listen must be 127.0.0.1:<port> or [::1]:<port> (a loopback address)";
    let addr: SocketAddr = text.trim().parse().map_err(|_| rule.to_owned())?;
    if !addr.ip().is_loopback() {
        return Err(rule.into());
    }
    Ok(addr)
}

/// The argv of `text` split on whitespace; its first word an absolute path.
///
/// # Errors
/// The rule it breaks.
pub fn parse_command(text: &str) -> Result<Vec<String>, String> {
    let argv: Vec<String> = text.split_whitespace().map(str::to_owned).collect();
    match argv.first() {
        Some(first) if Path::new(first).is_absolute() => Ok(argv),
        _ => Err("hive_mcp.hive_command must start with an absolute path".into()),
    }
}

/// Whether `actor` is a name the hive keeps as `created_by` and `--by`.
pub fn valid_actor(actor: &str) -> bool {
    !actor.is_empty()
        && actor.len() <= 64
        && !actor.starts_with('-')
        && actor
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:@-".contains(&b))
}

/// The trimmed token of the file at `path`, open to no group or other user.
///
/// # Errors
/// What is wrong with the file, never its content.
pub fn read_token(path: &Path) -> Result<String, String> {
    let at = path.display();
    let meta = fs::metadata(path).map_err(|e| format!("hive_mcp.token_file {at}: {e}"))?;
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(format!(
            "hive_mcp.token_file {at} is open to its group or other users; chmod 600 it"
        ));
    }
    let raw = fs::read(path).map_err(|e| format!("hive_mcp.token_file {at}: {e}"))?;
    let token = String::from_utf8(raw)
        .map_err(|_| format!("hive_mcp.token_file {at} is not text"))?
        .trim()
        .to_owned();
    let count = token.chars().count();
    if !(MIN_TOKEN_CHARS..=MAX_TOKEN_CHARS).contains(&count)
        || !token.bytes().all(|b| (0x21..=0x7e).contains(&b))
    {
        return Err(format!(
            "hive_mcp.token_file {at} must hold {MIN_TOKEN_CHARS} to {MAX_TOKEN_CHARS} visible ASCII characters"
        ));
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_must_be_loopback() {
        assert!(parse_listen("127.0.0.1:8740").is_ok());
        assert!(parse_listen("[::1]:8740").is_ok());
        assert!(parse_listen("0.0.0.0:8740").is_err());
        assert!(parse_listen("192.0.2.1:8740").is_err());
        assert!(parse_listen("localhost:8740").is_err());
        assert!(parse_listen("127.0.0.1").is_err());
    }

    #[test]
    fn command_starts_with_an_absolute_path() {
        assert_eq!(
            parse_command(" /usr/bin/python3  /x/hive.py ").ok(),
            Some(vec!["/usr/bin/python3".to_owned(), "/x/hive.py".to_owned()])
        );
        assert!(parse_command("python3 hive.py").is_err());
        assert!(parse_command("   ").is_err());
    }

    #[test]
    fn actors() {
        assert!(valid_actor("secretary"));
        assert!(valid_actor("cron:x"));
        assert!(!valid_actor("-x"));
        assert!(!valid_actor("a b"));
        assert!(!valid_actor(""));
    }
}
