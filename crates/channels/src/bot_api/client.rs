// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Blocking Bot API client: one call per method, the response envelope parsed into values.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value, json};
use ureq::{Agent, Body, SendBody, http::Response};

use super::config::Config;
use super::error::{Error, ErrorKind};
use super::multipart::Form;
use super::types::{BotUser, FileInfo, SentMessage, TextMessage, Update, Upload};

/// Every update type, `message_reaction` included, as the reference subscribes.
pub const ALL_UPDATE_TYPES: &[&str] = &[
    "message",
    "edited_message",
    "channel_post",
    "edited_channel_post",
    "inline_query",
    "chosen_inline_result",
    "callback_query",
    "shipping_query",
    "pre_checkout_query",
    "poll",
    "poll_answer",
    "my_chat_member",
    "chat_member",
    "chat_join_request",
    "chat_boost",
    "removed_chat_boost",
    "message_reaction",
    "message_reaction_count",
    "business_connection",
    "business_message",
    "edited_business_message",
    "deleted_business_messages",
    "purchased_paid_media",
    "managed_bot",
    "guest_message",
];

/// Largest JSON response read from the Bot API (a full `getUpdates` batch fits).
const MAX_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;
/// Captions longer than this are cut, as the reference does.
const MAX_CAPTION_CHARS: usize = 1024;
/// Per-write stall limit of an upload body.
const MEDIA_WRITE_TIMEOUT: Duration = Duration::from_secs(60);

/// Bot API client; cheap to clone and shareable between the poller and senders.
#[derive(Clone)]
pub struct Client {
    agent: Agent,
    config: Arc<Config>,
}

impl Client {
    /// A client for `config`; it opens no connection until the first call.
    pub fn new(config: Config) -> Self {
        let agent = Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_resolve(Some(config.connect_timeout))
            .timeout_connect(Some(config.connect_timeout))
            .timeout_per_write(Some(config.read_timeout))
            .timeout_recv_response(Some(config.read_timeout))
            .timeout_recv_body(Some(config.read_timeout))
            .build()
            .new_agent();
        Self {
            agent,
            config: Arc::new(config),
        }
    }

    /// The settings this client was built with.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// `getMe`: the bot's own account; fails with `Unauthorized` on a bad token.
    ///
    /// # Errors
    ///
    /// The classified [`Error`] of the call.
    pub fn get_me(&self) -> Result<BotUser, Error> {
        let result = self.call("getMe", &json!({}), None)?;
        Ok(BotUser {
            id: self.int_field(&result, "id")?,
            username: result
                .get("username")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }

    /// `deleteWebhook`: clears a webhook so polling works; optionally drops queued updates.
    ///
    /// # Errors
    ///
    /// The classified [`Error`] of the call.
    pub fn delete_webhook(&self, drop_pending_updates: bool) -> Result<(), Error> {
        let params = json!({ "drop_pending_updates": drop_pending_updates });
        self.call("deleteWebhook", &params, None).map(drop)
    }

    /// `getUpdates` long poll: blocks up to `timeout` on the server, plus the read timeout.
    ///
    /// # Errors
    ///
    /// The classified [`Error`] of the call; `InvalidResponse` for an update without `update_id`.
    pub fn get_updates(
        &self,
        offset: Option<i64>,
        timeout: Duration,
        allowed_updates: &[&str],
    ) -> Result<Vec<Update>, Error> {
        let mut params = json!({
            "timeout": timeout.as_secs(),
            "allowed_updates": allowed_updates,
        });
        if let Some(offset) = offset {
            params["offset"] = json!(offset);
        }
        let result = self.call("getUpdates", &params, Some(timeout))?;
        let Value::Array(items) = result else {
            return Err(self.error(
                ErrorKind::InvalidResponse,
                "getUpdates result is not a list",
            ));
        };
        items
            .into_iter()
            .map(|payload| {
                let update_id = self.int_field(&payload, "update_id")?;
                Ok(Update { update_id, payload })
            })
            .collect()
    }

    /// `sendMessage`, once; retries live in [`super::Sender`].
    ///
    /// # Errors
    ///
    /// The classified [`Error`] of the call; Telegram's description is kept.
    pub fn send_message(&self, message: &TextMessage) -> Result<SentMessage, Error> {
        let mut params = json!({ "chat_id": message.chat_id, "text": message.text });
        if let Some(mode) = &message.parse_mode {
            params["parse_mode"] = json!(mode);
        }
        if let Some(reply_to) = message.reply_to {
            params["reply_parameters"] = json!({ "message_id": reply_to });
        }
        if self.config.disable_link_previews {
            params["link_preview_options"] = json!({ "is_disabled": true });
        }
        if message.disable_notification {
            params["disable_notification"] = json!(true);
        }
        let result = self.call("sendMessage", &params, None)?;
        self.sent(&result)
    }

    /// `sendChatAction` with `action` (for example `typing`).
    ///
    /// # Errors
    ///
    /// The classified [`Error`] of the call.
    pub fn send_chat_action(&self, chat_id: i64, action: &str) -> Result<(), Error> {
        let params = json!({ "chat_id": chat_id, "action": action });
        self.call("sendChatAction", &params, None).map(drop)
    }

    /// `setMessageReaction`: sets one emoji, or clears the bot's reactions with `None`.
    ///
    /// # Errors
    ///
    /// The classified [`Error`] of the call.
    pub fn set_message_reaction(
        &self,
        chat_id: i64,
        message_id: i64,
        emoji: Option<&str>,
    ) -> Result<(), Error> {
        let mut params = json!({ "chat_id": chat_id, "message_id": message_id });
        if let Some(emoji) = emoji {
            params["reaction"] = json!([{ "type": "emoji", "emoji": emoji }]);
        }
        self.call("setMessageReaction", &params, None).map(drop)
    }

    /// `getFile`: the path to download a file from.
    ///
    /// # Errors
    ///
    /// The classified [`Error`] of the call.
    pub fn get_file(&self, file_id: &str) -> Result<FileInfo, Error> {
        let result = self.call("getFile", &json!({ "file_id": file_id }), None)?;
        Ok(FileInfo {
            file_id: result
                .get("file_id")
                .and_then(Value::as_str)
                .unwrap_or(file_id)
                .to_owned(),
            file_size: result.get("file_size").and_then(Value::as_u64),
            file_path: result
                .get("file_path")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }

    /// Streams `file` into `out`; refuses files over `max_file_bytes`. Returns the byte count.
    ///
    /// # Errors
    ///
    /// `TooLarge` over the limit, `Io` when `out` fails, else the classified transport error.
    pub fn download(&self, file: &FileInfo, out: &mut dyn Write) -> Result<u64, Error> {
        let limit = self.config.max_file_bytes;
        if file.file_size.is_some_and(|size| size > limit) {
            return Err(self.error(
                ErrorKind::TooLarge,
                "file is larger than the download limit",
            ));
        }
        let Some(path) = &file.file_path else {
            return Err(self.error(ErrorKind::InvalidResponse, "file has no file_path"));
        };
        self.download_path(path, limit, out)
    }

    /// Streams the file at `path` under the file base into `out`, failing once more than
    /// `limit` bytes arrive. Returns the byte count.
    pub(crate) fn download_path(
        &self,
        path: &str,
        limit: u64,
        out: &mut dyn Write,
    ) -> Result<u64, Error> {
        let url = format!(
            "{}{}/{}",
            self.config.file_base_url, self.config.token, path
        );
        // The reference's read timeout bounds each read, not the whole body; ureq's
        // `timeout_recv_body` is a total budget, so a large file still arriving would fail.
        let mut response = self
            .agent
            .get(&url)
            .config()
            .timeout_recv_body(None)
            .timeout_per_read(Some(self.config.read_timeout))
            .build()
            .call()
            .map_err(|e| self.transport_error(e))?;
        if !response.status().is_success() {
            return Err(self.status_error(&mut response));
        }
        let mut reader = response.body_mut().with_config().limit(u64::MAX).reader();
        let mut buf = vec![0; 64 * 1024];
        let mut copied: u64 = 0;
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => return Ok(copied),
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(self.transport_error(e.into())),
            };
            copied += n as u64;
            if copied > limit {
                return Err(self.error(
                    ErrorKind::TooLarge,
                    "file is larger than the download limit",
                ));
            }
            out.write_all(&buf[..n])
                .map_err(|e| self.error(ErrorKind::Io, &format!("write: {e}")))?;
        }
    }

    /// Downloads `file` into a new file at `dest` with mode 0600; removes it on failure.
    ///
    /// # Errors
    ///
    /// As [`Client::download`], and `Io` when `dest` exists or cannot be created.
    pub fn download_to_path(&self, file: &FileInfo, dest: &Path) -> Result<u64, Error> {
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dest)
            .map_err(|e| self.error(ErrorKind::Io, &format!("create: {e}")))?;
        let result = self
            .download(file, &mut out)
            .and_then(|n| match out.sync_all() {
                Ok(()) => Ok(n),
                Err(e) => Err(self.error(ErrorKind::Io, &format!("sync: {e}"))),
            });
        if result.is_err() {
            drop(out);
            // A partial file must not look like a finished attachment.
            let _ = fs::remove_file(dest);
        }
        result
    }

    /// `sendDocument`, `sendPhoto` or `sendVoice` with the file streamed from disk.
    ///
    /// # Errors
    ///
    /// `Io` when the file cannot be read, else the classified [`Error`] of the call.
    pub fn send_media(&self, upload: &Upload) -> Result<SentMessage, Error> {
        let file = File::open(&upload.path)
            .map_err(|e| self.error(ErrorKind::Io, &format!("open upload: {e}")))?;
        let name = upload.file_name.clone().unwrap_or_else(|| {
            upload
                .path
                .file_name()
                .map_or_else(|| "file".to_owned(), |n| n.to_string_lossy().into_owned())
        });
        let mut fields = vec![("chat_id", upload.chat_id.to_string())];
        if let Some(caption) = &upload.caption {
            fields.push(("caption", caption.chars().take(MAX_CAPTION_CHARS).collect()));
        }
        if let Some(mode) = &upload.parse_mode {
            fields.push(("parse_mode", mode.clone()));
        }
        if let Some(reply_to) = upload.reply_to {
            fields.push((
                "reply_parameters",
                json!({ "message_id": reply_to }).to_string(),
            ));
        }
        if upload.disable_notification {
            fields.push(("disable_notification", "true".to_owned()));
        }
        let form = Form::new(
            &fields,
            upload.kind.field(),
            &name,
            "application/octet-stream",
            file,
        )
        .map_err(|e| self.error(ErrorKind::Io, &format!("read upload: {e}")))?;
        let request = self
            .agent
            .post(self.method_url(upload.kind.method()))
            .header("Content-Type", form.content_type())
            .header("Content-Length", form.len().to_string())
            .config()
            .timeout_per_write(Some(MEDIA_WRITE_TIMEOUT))
            .timeout_recv_response(Some(self.config.media_read_timeout))
            .build();
        let response = request
            .send(SendBody::from_owned_reader(form.into_reader()))
            .map_err(|e| self.transport_error(e))?;
        let result = self.envelope(response)?;
        self.sent(&result)
    }

    fn method_url(&self, method: &str) -> String {
        format!("{}{}/{}", self.config.base_url, self.config.token, method)
    }

    /// POSTs `params` as JSON; `long_poll` extends the response wait by the poll time.
    fn call(
        &self,
        method: &str,
        params: &Value,
        long_poll: Option<Duration>,
    ) -> Result<Value, Error> {
        let wait = self.config.read_timeout + long_poll.unwrap_or_default();
        let response = self
            .agent
            .post(self.method_url(method))
            .header("Content-Type", "application/json")
            .config()
            .timeout_recv_response(Some(wait))
            .build()
            .send(params.to_string())
            .map_err(|e| self.transport_error(e))?;
        self.envelope(response)
    }

    /// Unwraps `{ok, result}`; a failure keeps Telegram's description and parameters.
    fn envelope(&self, mut response: Response<Body>) -> Result<Value, Error> {
        if !response.status().is_success() {
            return Err(self.status_error(&mut response));
        }
        let body = self.read_json(&mut response)?;
        match body {
            Some(Value::Object(mut map)) if map.get("ok") == Some(&Value::Bool(true)) => map
                .remove("result")
                .ok_or_else(|| self.error(ErrorKind::InvalidResponse, "response has no result")),
            Some(Value::Object(map)) => Err(self.error(
                ErrorKind::InvalidResponse,
                description(&map).unwrap_or("invalid server response"),
            )),
            _ => Err(self.error(ErrorKind::InvalidResponse, "invalid server response")),
        }
    }

    /// Maps a non-2xx response the way the reference's Telegram library does.
    fn status_error(&self, response: &mut Response<Body>) -> Error {
        let status = response.status().as_u16();
        let fallback = format!("HTTP status {status}");
        let body = self.read_json(response).ok().flatten();
        let map = match &body {
            Some(Value::Object(map)) => Some(map),
            _ => None,
        };
        let text = map.and_then(description).unwrap_or(&fallback);
        let parameters = map.and_then(|m| m.get("parameters"));
        if let Some(chat) = parameters
            .and_then(|p| p.get("migrate_to_chat_id"))
            .and_then(Value::as_i64)
            .filter(|id| *id != 0)
        {
            return self.error(ErrorKind::ChatMigrated(chat), text);
        }
        // A wait that does not fit a `Duration` (hostile or broken server) is ignored and the
        // answer falls through to the status mapping instead of panicking the thread.
        if let Some(wait) = parameters
            .and_then(|p| p.get("retry_after"))
            .and_then(Value::as_f64)
            .filter(|s| *s > 0.0)
            .and_then(|s| Duration::try_from_secs_f64(s).ok())
        {
            return self.error(ErrorKind::RetryAfter(wait), text);
        }
        let kind = match status {
            400 => ErrorKind::BadRequest,
            401 | 404 => ErrorKind::Unauthorized,
            403 => ErrorKind::Forbidden,
            409 => ErrorKind::Conflict,
            other => ErrorKind::Server(other),
        };
        self.error(kind, text)
    }

    fn read_json(&self, response: &mut Response<Body>) -> Result<Option<Value>, Error> {
        let bytes = response
            .body_mut()
            .with_config()
            .limit(MAX_RESPONSE_BYTES)
            .read_to_vec()
            .map_err(|e| self.transport_error(e))?;
        Ok(serde_json::from_slice(&bytes).ok())
    }

    fn sent(&self, result: &Value) -> Result<SentMessage, Error> {
        Ok(SentMessage {
            message_id: self.int_field(result, "message_id")?,
        })
    }

    fn int_field(&self, value: &Value, field: &str) -> Result<i64, Error> {
        value.get(field).and_then(Value::as_i64).ok_or_else(|| {
            self.error(
                ErrorKind::InvalidResponse,
                &format!("result has no integer {field}"),
            )
        })
    }

    /// A transport failure; ureq's own text is not used because it may contain the URL.
    fn transport_error(&self, err: ureq::Error) -> Error {
        use ureq::Timeout;
        let (kind, text) = match err {
            ureq::Error::Timeout(Timeout::Resolve | Timeout::Connect) => {
                (ErrorKind::ConnectTimeout, "connect timed out".to_owned())
            }
            ureq::Error::Timeout(which) => (ErrorKind::Timeout, format!("timed out: {which}")),
            ureq::Error::Io(e) if e.kind() == io::ErrorKind::TimedOut => {
                (ErrorKind::Timeout, "timed out".to_owned())
            }
            ureq::Error::Io(e) => (ErrorKind::Network, format!("network: {}", e.kind())),
            ureq::Error::HostNotFound => (ErrorKind::Network, "host not found".to_owned()),
            ureq::Error::ConnectionFailed => (ErrorKind::Network, "connection failed".to_owned()),
            ureq::Error::BodyExceedsLimit(_) => {
                (ErrorKind::InvalidResponse, "response too large".to_owned())
            }
            _ => (
                ErrorKind::Network,
                "network: malformed HTTP exchange".to_owned(),
            ),
        };
        self.error(kind, &text)
    }

    fn error(&self, kind: ErrorKind, text: &str) -> Error {
        Error::new(kind, text, &self.config.token)
    }
}

fn description(map: &Map<String, Value>) -> Option<&str> {
    map.get("description")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}
