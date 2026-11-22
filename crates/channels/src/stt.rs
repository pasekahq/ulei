// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Speech to text for voice notes through an OpenAI-compatible `/audio/transcriptions`
//! endpoint, the reference's cloud Whisper path (ADR-0035). Error texts are the reference's.

use std::error::Error;
use std::fmt::{self, Write as _};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use ulei_contracts::env_file;
use ureq::{Agent, SendBody};

use crate::bot_api::multipart::{FilePart, Form};
use crate::bot_api::redact;
use crate::ingress::Transcriber;
use crate::render::pytext::{strip, suffix};

/// Endpoint base when `telegram.stt_base_url` is unset.
pub const DEFAULT_BASE_URL: &str = "https://api.groq.com/openai/v1";
/// Model when `telegram.stt_model` is unset.
pub const DEFAULT_MODEL: &str = "whisper-large-v3-turbo";
/// Env-file variable holding the endpoint key.
pub const KEY_NAME: &str = "GROQ_API_KEY";
/// Largest file the endpoint accepts.
pub const MAX_FILE_BYTES: u64 = 25 * 1024 * 1024;
/// Bound of each connect, read and write of a call.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Models only the `OpenAI` endpoint serves; a request for one uses [`DEFAULT_MODEL`].
const OPENAI_MODELS: &[&str] = &[
    "whisper-1",
    "gpt-4o-mini-transcribe",
    "gpt-4o-transcribe",
    "gpt-transcribe",
];

/// Accepted extensions in sorted order, each with the content type its upload carries.
const FORMATS: &[(&str, &str)] = &[
    (".aac", "audio/aac"),
    (".caf", "application/octet-stream"),
    (".flac", "audio/flac"),
    (".m4a", "audio/mp4"),
    (".mp3", "audio/mpeg"),
    (".mp4", "video/mp4"),
    (".mpeg", "video/mpeg"),
    (".mpga", "audio/mpeg"),
    (".oga", "audio/ogg"),
    (".ogg", "audio/ogg"),
    (".opus", "audio/ogg"),
    (".wav", "audio/x-wav"),
    (".webm", "video/webm"),
];

/// Settings of [`Stt`]. The key is not among them: each call reads it from `env_file`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SttConfig {
    /// Endpoint base; the call goes to `<base_url>/audio/transcriptions`.
    pub base_url: String,
    /// Model sent with each file.
    pub model: String,
    /// Env-file holding [`KEY_NAME`].
    pub env_file: PathBuf,
    /// Bound of each connect, read and write of a call.
    pub timeout: Duration,
}

impl SttConfig {
    /// The defaults with the key in `env_file`.
    pub fn new(env_file: PathBuf) -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_owned(),
            model: DEFAULT_MODEL.to_owned(),
            env_file,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

/// Why a file was not transcribed: the reference's error text, never carrying the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SttError(String);

impl fmt::Display for SttError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for SttError {}

/// The transcriber: one call per file, no retries.
pub struct Stt {
    agent: Agent,
    config: SttConfig,
}

impl Stt {
    /// A transcriber for `config`; it opens no connection until the first file.
    pub fn new(config: SttConfig) -> Self {
        let t = Some(config.timeout);
        let agent = Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_resolve(t)
            .timeout_connect(t)
            .timeout_per_write(t)
            .timeout_per_read(t)
            .build()
            .new_agent();
        Self { agent, config }
    }

    /// Transcript of the audio file at `path`, stripped; empty when the endpoint heard no words.
    ///
    /// # Errors
    /// The file is a symbolic link, missing, not a regular file, of an unsupported format, over
    /// [`MAX_FILE_BYTES`] or unreadable; the key is not set; the endpoint is unreachable, timed
    /// out or answered with an error status.
    pub fn transcribe_file(&self, path: &Path) -> Result<String, SttError> {
        let shown = path.display();
        let mime = check_file(path)?;
        let key = self.key()?;
        let model = if OPENAI_MODELS.contains(&self.config.model.as_str()) {
            DEFAULT_MODEL
        } else {
            self.config.model.as_str()
        };
        let file = File::open(path).map_err(|e| match e.kind() {
            io::ErrorKind::PermissionDenied => fail(format!("Permission denied: {shown}")),
            _ => fail(format!("Transcription failed: {e}")),
        })?;
        let name = path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let fields = [
            ("model", model.to_owned()),
            ("response_format", "text".to_owned()),
        ];
        let part = FilePart {
            field: "file".to_owned(),
            name,
            file_type: mime,
            file,
        };
        let form = Form::new(&fields, vec![part])
            .map_err(|e| fail(format!("Transcription failed: {e}")))?;
        let url = format!(
            "{}/audio/transcriptions",
            self.config.base_url.trim_end_matches('/')
        );
        let mut response = self
            .agent
            .post(&url)
            .header("Authorization", format!("Bearer {key}"))
            .header("Content-Type", form.content_type())
            .header("Content-Length", form.len().to_string())
            .send(SendBody::from_owned_reader(form.into_reader()))
            .map_err(|e| connection_error(&e))?;
        let status = response.status();
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|e| connection_error(&e))?;
        if !status.is_success() {
            let message = status_message(status.as_u16(), &body);
            return Err(fail(format!("API error: {}", redact(&message, &key))));
        }
        Ok(strip(&body).to_owned())
    }

    /// [`KEY_NAME`] from the env-file; its last assignment wins.
    fn key(&self) -> Result<String, SttError> {
        let path = &self.config.env_file;
        let entries = env_file::read(path)
            .map_err(|e| fail(format!("cannot read the env-file {}: {e}", path.display())))?;
        let key = entries
            .into_iter()
            .rev()
            .find(|(name, _)| name == KEY_NAME)
            .map(|(_, value)| value);
        match key.as_deref().map(strip) {
            Some(key) if !key.is_empty() => Ok(key.to_owned()),
            _ => Err(fail(format!("{KEY_NAME} not set"))),
        }
    }
}

impl Transcriber for Stt {
    fn transcribe(&self, path: &Path) -> Result<String, Box<dyn Error + Send + Sync>> {
        self.transcribe_file(path).map_err(Into::into)
    }
}

impl fmt::Debug for Stt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Stt")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

fn fail(text: String) -> SttError {
    SttError(text)
}

/// The checks before upload, in the reference's order; the upload's content type.
fn check_file(path: &Path) -> Result<&'static str, SttError> {
    let shown = path.display();
    if path.is_symlink() {
        return Err(fail(format!("Path is a symbolic link: {shown}")));
    }
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(fail(format!("Audio file not found: {shown}")));
        }
        Err(e) => return Err(fail(format!("Failed to access file: {e}"))),
    };
    if !meta.is_file() {
        return Err(fail(format!("Path is not a file: {shown}")));
    }
    let name = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let suffix = suffix(&name);
    let lower = suffix.to_lowercase();
    let Some(&(ext, mime)) = FORMATS.iter().find(|(ext, _)| *ext == lower) else {
        let supported: Vec<&str> = FORMATS.iter().map(|(ext, _)| *ext).collect();
        return Err(fail(format!(
            "Unsupported format: {suffix}. Supported: {}",
            supported.join(", ")
        )));
    };
    let size = meta.len();
    if size > MAX_FILE_BYTES {
        #[allow(clippy::cast_precision_loss)]
        let (mb, max) = (
            size as f64 / (1024.0 * 1024.0),
            MAX_FILE_BYTES as f64 / (1024.0 * 1024.0),
        );
        return Err(fail(format!("File too large: {mb:.1}MB (max {max:.0}MB)")));
    }
    if ext == ".caf" {
        return Err(fail("CAF audio could not be converted to WAV.".to_owned()));
    }
    Ok(mime)
}

fn connection_error(err: &ureq::Error) -> SttError {
    let timed_out = match err {
        ureq::Error::Timeout(_) => true,
        ureq::Error::Io(e) => e.kind() == io::ErrorKind::TimedOut,
        _ => false,
    };
    if timed_out {
        fail("Connection error: Request timed out.".to_owned())
    } else {
        fail("Connection error: Connection error.".to_owned())
    }
}

/// The client library's text of an error status: the code and the parsed JSON body, else the
/// body text, else the code alone.
fn status_message(status: u16, body: &str) -> String {
    let text = strip(body);
    match serde_json::from_str::<PyRepr>(text) {
        Ok(PyRepr(repr)) => format!("Error code: {status} - {repr}"),
        Err(_) if text.is_empty() => format!("Error code: {status}"),
        Err(_) => text.to_owned(),
    }
}

/// `repr()` of the object a JSON text parses to, keys in their order in the text.
struct PyRepr(String);

impl<'de> Deserialize<'de> for PyRepr {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(PyReprVisitor).map(PyRepr)
    }
}

struct PyReprVisitor;

impl<'de> Visitor<'de> for PyReprVisitor {
    type Value = String;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_unit<E>(self) -> Result<String, E> {
        Ok("None".to_owned())
    }

    fn visit_bool<E>(self, v: bool) -> Result<String, E> {
        Ok(if v { "True" } else { "False" }.to_owned())
    }

    fn visit_i64<E>(self, v: i64) -> Result<String, E> {
        Ok(v.to_string())
    }

    fn visit_u64<E>(self, v: u64) -> Result<String, E> {
        Ok(v.to_string())
    }

    fn visit_f64<E>(self, v: f64) -> Result<String, E> {
        Ok(if v.is_finite() && v.fract() == 0.0 && v.abs() < 1e16 {
            format!("{v:.1}")
        } else {
            v.to_string()
        })
    }

    fn visit_str<E>(self, v: &str) -> Result<String, E> {
        Ok(str_repr(v))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<String, A::Error> {
        let mut items = Vec::new();
        while let Some(PyRepr(item)) = seq.next_element()? {
            items.push(item);
        }
        Ok(format!("[{}]", items.join(", ")))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<String, A::Error> {
        let mut entries = Vec::new();
        while let Some((key, PyRepr(value))) = map.next_entry::<String, PyRepr>()? {
            entries.push(format!("{}: {value}", str_repr(&key)));
        }
        Ok(format!("{{{}}}", entries.join(", ")))
    }
}

/// `repr()` of a string: single quotes unless only double quotes avoid escaping; backslash,
/// quote and control characters escaped.
fn str_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() && u32::from(c) <= 0xff => {
                let _ = write!(out, "\\x{:02x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}
