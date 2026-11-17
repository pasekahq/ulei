// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Settings of the Bot API client; the binary fills them from its config file.

use std::fmt;
use std::time::Duration;

/// Default method base: the public Bot API; a method URL is `{base_url}{token}/{method}`.
pub const DEFAULT_BASE_URL: &str = "https://api.telegram.org/bot";
/// Default file base: a file URL is `{file_base_url}{token}/{file_path}`.
pub const DEFAULT_FILE_BASE_URL: &str = "https://api.telegram.org/file/bot";

/// `getFile` limit of the public Bot API (20 MiB).
const PUBLIC_MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;
/// Limit of a self-hosted Bot API server (2 GiB).
const LOCAL_MAX_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Bot API client settings. `Debug` never shows the token.
#[derive(Clone)]
pub struct Config {
    /// Bot token; a secret, it travels only in request paths.
    pub token: String,
    /// Method base, ending where the token starts (for example `http://127.0.0.1:8081/bot`).
    pub base_url: String,
    /// File base, ending where the token starts.
    pub file_base_url: String,
    /// Resolve and connect limit of every request.
    pub connect_timeout: Duration,
    /// Response limit of an ordinary call; a long poll adds its poll timeout.
    pub read_timeout: Duration,
    /// Response limit of an upload (the request body may be large).
    pub media_read_timeout: Duration,
    /// Seconds Telegram holds one `getUpdates` open.
    pub poll_timeout: Duration,
    /// Largest file `download` accepts.
    pub max_file_bytes: u64,
    /// Sends `link_preview_options {is_disabled: true}` with every text message.
    pub disable_link_previews: bool,
}

impl Config {
    /// Settings with the reference defaults; a non-default `base_url` (a self-hosted server)
    /// raises the download limit from 20 MiB to 2 GiB, and `file_base_url` follows it.
    pub fn new(token: impl Into<String>, base_url: Option<&str>) -> Self {
        let (base_url, file_base_url, max_file_bytes) = match base_url {
            Some(url) if url != DEFAULT_BASE_URL => {
                (url.to_owned(), url.to_owned(), LOCAL_MAX_FILE_BYTES)
            }
            _ => (
                DEFAULT_BASE_URL.to_owned(),
                DEFAULT_FILE_BASE_URL.to_owned(),
                PUBLIC_MAX_FILE_BYTES,
            ),
        };
        Self {
            token: token.into(),
            base_url,
            file_base_url,
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(20),
            media_read_timeout: Duration::from_secs(60),
            poll_timeout: Duration::from_secs(10),
            max_file_bytes,
            disable_link_previews: false,
        }
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("token", &"***")
            .field("base_url", &self.base_url)
            .field("file_base_url", &self.file_base_url)
            .field("connect_timeout", &self.connect_timeout)
            .field("read_timeout", &self.read_timeout)
            .field("media_read_timeout", &self.media_read_timeout)
            .field("poll_timeout", &self.poll_timeout)
            .field("max_file_bytes", &self.max_file_bytes)
            .field("disable_link_previews", &self.disable_link_previews)
            .finish()
    }
}
