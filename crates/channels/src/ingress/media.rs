// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! Attachments: download through [`FileSource`] into the inbox directory (mode 0600, unique
//! names), classification, size limits and the notes the model sees about them.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::text::sanitize_display_name;

/// Where Telegram files come from: `getFile` plus a bounded download.
///
/// Seam: the Bot API transport implements it.
pub trait FileSource {
    /// Resolves a `file_id` (`getFile`).
    ///
    /// # Errors
    /// The Bot API call failed.
    fn locate(&self, file_id: &str) -> io::Result<RemoteFile>;

    /// Streams the file into `out`, failing once more than `max_bytes` arrive; returns the
    /// number of bytes written.
    ///
    /// # Errors
    /// The download failed or exceeded `max_bytes`.
    fn download(&self, file: &RemoteFile, max_bytes: u64, out: &mut dyn Write) -> io::Result<u64>;
}

/// A file resolved by `getFile`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFile {
    /// The Bot API `file_path` (its extension hints the format).
    pub file_path: String,
}

/// What an attachment is, for routing in the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentKind {
    /// Picture.
    Image,
    /// Voice note or audio file.
    Audio,
    /// Video.
    Video,
    /// Any other file.
    Document,
}

/// A file saved in the inbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// Absolute path of the saved file.
    pub path: PathBuf,
    /// Kind.
    pub kind: AttachmentKind,
    /// MIME type recorded for it.
    pub mime: String,
    /// Size in bytes.
    pub size: u64,
    /// Duration in seconds Telegram reported (voice, audio, video).
    pub duration_secs: Option<u64>,
}

/// A failed download, named by its class in the note the model and the owner see.
#[derive(Debug)]
pub(crate) enum FetchError {
    Io(io::Error),
    NotAnImage,
}

impl FetchError {
    /// Short class name, as the reference shows the exception class.
    pub(crate) fn class(&self) -> String {
        match self {
            Self::Io(e) => format!("{:?}", e.kind()),
            Self::NotAnImage => "ValueError".to_owned(),
        }
    }
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "download failed: {e}"),
            Self::NotAnImage => f.write_str("downloaded data is not an image"),
        }
    }
}

impl From<io::Error> for FetchError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// A downloaded file that has not got its final name yet.
pub(crate) struct Fetched {
    part: PathBuf,
    pub(crate) file_path: String,
    pub(crate) size: u64,
}

/// The inbox directory and its name generator.
pub(crate) struct Inbox {
    dir: PathBuf,
    seq: u64,
}

impl Inbox {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self { dir, seq: 0 }
    }

    fn token(&mut self) -> String {
        self.seq += 1;
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let mut x = u64::try_from(nanos & u128::from(u64::MAX)).unwrap_or(0)
            ^ (u64::from(std::process::id()) << 32)
            ^ self.seq.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        // splitmix64 finaliser: spreads close inputs over all bits.
        x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^= x >> 31;
        format!("{:012x}", x & 0xFFFF_FFFF_FFFF)
    }

    /// Downloads `file_id` into a private part file of the inbox.
    pub(crate) fn fetch(
        &mut self,
        files: &dyn FileSource,
        file_id: &str,
        max_bytes: u64,
    ) -> Result<Fetched, FetchError> {
        let remote = files.locate(file_id)?;
        // Saved names include the owner's document names: keep the inbox private.
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)?;
        let (part, mut out) = loop {
            let name = format!(".part_{}", self.token());
            let part = self.dir.join(name);
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&part)
            {
                Ok(f) => break (part, f),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
        };
        let written = files
            .download(&remote, max_bytes, &mut out)
            .and_then(|n| out.flush().map(|()| n));
        match written {
            Ok(size) => Ok(Fetched {
                part,
                file_path: remote.file_path,
                size,
            }),
            Err(e) => {
                let _ = fs::remove_file(&part);
                Err(e.into())
            }
        }
    }

    /// Gives a fetched file its final name `<prefix>_<token><tail>` without replacing any file.
    pub(crate) fn keep(
        &mut self,
        fetched: &Fetched,
        prefix: &str,
        tail: &str,
    ) -> io::Result<PathBuf> {
        loop {
            let name = format!("{prefix}_{}{tail}", self.token());
            let path = self.dir.join(name);
            match fs::hard_link(&fetched.part, &path) {
                Ok(()) => {
                    fs::remove_file(&fetched.part)?;
                    return Ok(path);
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
    }

    pub(crate) fn discard(fetched: &Fetched) {
        let _ = fs::remove_file(&fetched.part);
    }
}

/// First bytes of a fetched file, for format sniffing.
pub(crate) fn head(fetched: &Fetched) -> Vec<u8> {
    let mut buf = Vec::with_capacity(16);
    if let Ok(f) = File::open(&fetched.part) {
        let _ = f.take(16).read_to_end(&mut buf);
    }
    buf
}

/// Reads a fetched file whole when it is at most `max` bytes.
pub(crate) fn read_small(path: &Path, max: u64) -> Option<Vec<u8>> {
    let f = File::open(path).ok()?;
    let mut buf = Vec::new();
    f.take(max + 1).read_to_end(&mut buf).ok()?;
    (buf.len() as u64 <= max).then_some(buf)
}

/// The data starts with a PNG, JPEG, GIF, BMP or WEBP signature.
pub(crate) fn looks_like_image(data: &[u8]) -> bool {
    data.len() >= 4
        && (data.starts_with(b"\x89PNG\r\n\x1a\n")
            || data.starts_with(b"\xff\xd8\xff")
            || data.starts_with(b"GIF87a")
            || data.starts_with(b"GIF89a")
            || data.starts_with(b"BM")
            || (data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP"))
}

/// Extension of the audio container the data starts with, else `fallback`.
pub(crate) fn sniff_audio_ext<'a>(data: &[u8], fallback: &'a str) -> &'a str {
    if data.len() >= 8 && &data[4..8] == b"ftyp" {
        return ".m4a";
    }
    if data.starts_with(b"OggS") {
        return ".ogg";
    }
    if data.starts_with(b"fLaC") {
        return ".flac";
    }
    if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WAVE" {
        return ".wav";
    }
    if data.starts_with(b"ID3") {
        return ".mp3";
    }
    if data.len() >= 2 && data[0] == 0xFF && (data[1] & 0xE0) == 0xE0 {
        return if (data[1] & 0xF6) == 0xF0 {
            ".aac"
        } else {
            ".mp3"
        };
    }
    if data.starts_with(b"\x1a\x45\xdf\xa3") {
        return ".webm";
    }
    fallback
}

/// Image extensions Telegram may deliver as documents.
pub(crate) const IMAGE_EXTENSIONS: &[&str] = &[".png", ".jpg", ".jpeg", ".webp", ".gif"];

pub(crate) fn image_mime_to_ext(mime: &str) -> Option<&'static str> {
    Some(match mime {
        "image/png" => ".png",
        "image/jpeg" | "image/jpg" => ".jpg",
        "image/webp" => ".webp",
        "image/gif" => ".gif",
        _ => return None,
    })
}

pub(crate) fn image_ext_to_mime(ext: &str) -> &'static str {
    match ext {
        ".png" => "image/png",
        ".webp" => "image/webp",
        ".gif" => "image/gif",
        _ => "image/jpeg",
    }
}

pub(crate) const VIDEO_TYPES: &[(&str, &str)] = &[
    (".mp4", "video/mp4"),
    (".mov", "video/quicktime"),
    (".webm", "video/webm"),
    (".mkv", "video/x-matroska"),
    (".avi", "video/x-msvideo"),
];

pub(crate) const DOCUMENT_TYPES: &[(&str, &str)] = &[
    (".pdf", "application/pdf"),
    (".md", "text/markdown"),
    (".txt", "text/plain"),
    (".csv", "text/csv"),
    (".log", "text/plain"),
    (".json", "application/json"),
    (".xml", "application/xml"),
    (".yaml", "application/yaml"),
    (".yml", "application/yaml"),
    (".toml", "application/toml"),
    (".ini", "text/plain"),
    (".cfg", "text/plain"),
    (".zip", "application/zip"),
    (".doc", "application/msword"),
    (
        ".docx",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    ),
    (".xls", "application/vnd.ms-excel"),
    (
        ".xlsx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    ),
    (".ppt", "application/vnd.ms-powerpoint"),
    (
        ".pptx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    ),
    (".ts", "text/plain"),
    (".py", "text/plain"),
    (".sh", "text/plain"),
];

pub(crate) const AUDIO_TYPES: &[(&str, &str)] = &[
    (".ogg", "audio/ogg"),
    (".opus", "audio/opus"),
    (".mp3", "audio/mpeg"),
    (".m2a", "audio/mpeg"),
    (".wav", "audio/wav"),
    (".m4a", "audio/m4a"),
    (".flac", "audio/flac"),
];

/// Extensions whose content is inlined into the turn when small and UTF-8.
pub(crate) const TEXT_INJECT_EXTENSIONS: &[&str] = &[
    ".txt",
    ".md",
    ".markdown",
    ".csv",
    ".tsv",
    ".log",
    ".json",
    ".jsonl",
    ".ndjson",
    ".xml",
    ".yaml",
    ".yml",
    ".toml",
    ".ini",
    ".cfg",
    ".conf",
    ".env",
    ".properties",
    ".html",
    ".htm",
    ".css",
    ".scss",
    ".sass",
    ".less",
    ".py",
    ".pyi",
    ".js",
    ".mjs",
    ".cjs",
    ".ts",
    ".tsx",
    ".jsx",
    ".sh",
    ".bash",
    ".zsh",
    ".fish",
    ".ps1",
    ".bat",
    ".c",
    ".h",
    ".cpp",
    ".cc",
    ".hpp",
    ".cs",
    ".java",
    ".kt",
    ".go",
    ".rs",
    ".rb",
    ".php",
    ".pl",
    ".lua",
    ".r",
    ".jl",
    ".swift",
    ".m",
    ".scala",
    ".clj",
    ".ex",
    ".exs",
    ".erl",
    ".sql",
    ".graphql",
    ".proto",
    ".tf",
    ".hcl",
    ".dockerfile",
    ".makefile",
    ".cmake",
    ".gradle",
    ".rst",
    ".tex",
    ".srt",
    ".vtt",
    ".diff",
    ".patch",
];

pub(crate) fn lookup<'a>(table: &[(&str, &'a str)], ext: &str) -> Option<&'a str> {
    table.iter().find(|(e, _)| *e == ext).map(|(_, m)| *m)
}

pub(crate) fn reverse_lookup(table: &[(&'static str, &str)], mime: &str) -> Option<&'static str> {
    table.iter().find(|(_, m)| *m == mime).map(|(e, _)| *e)
}

/// Lower-cased extension with the dot, or `""`.
pub(crate) fn extension(name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name);
    match base.rfind('.') {
        Some(i) if i > 0 && base[..i].chars().any(|c| c != '.') => base[i..].to_lowercase(),
        _ => String::new(),
    }
}

/// Last path component with NUL removed; `document` when nothing usable is left.
pub(crate) fn safe_file_name(name: &str) -> String {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .replace('\0', "");
    let base = base.trim();
    if base.is_empty() || base == "." || base == ".." {
        "document".to_owned()
    } else {
        base.to_owned()
    }
}

/// `[Telegram <label> skipped: file size N MB exceeds the L MB limit. ...]`.
pub(crate) fn too_large_note(label: &str, size: u64, max_bytes: u64) -> String {
    let limit_mb = (max_bytes / (1024 * 1024)).max(1);
    #[allow(clippy::cast_precision_loss)]
    let size_mb = size as f64 / (1024.0 * 1024.0);
    format!(
        "[Telegram {label} skipped: file size {size_mb:.1} MB exceeds the {limit_mb} MB limit. \
         Ask the user to send a smaller file.]"
    )
}

/// The note appended when an attachment could not be downloaded.
pub(crate) fn cache_failure_note(kind: &str, display_name: Option<&str>, class: &str) -> String {
    let named = display_name.map(|n| format!(" ({n})")).unwrap_or_default();
    format!(
        "[The user attempted to send a {kind}{named} but it could not be downloaded ({class}); \
         they have been asked to retry.]"
    )
}

/// The message sent back to the owner when an attachment could not be downloaded.
pub(crate) fn cache_failure_reply(kind: &str, display_name: Option<&str>, class: &str) -> String {
    let named = display_name.map(|n| format!(" ({n})")).unwrap_or_default();
    format!(
        "\u{26a0}\u{fe0f} Couldn't download your {kind}{named} ({class}). Please try sending it again."
    )
}

/// Name the model sees for a saved file: what follows `<prefix>_<token>_`, sanitised.
pub(crate) fn display_name_of(path: &Path) -> String {
    let base = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut parts = base.splitn(3, '_');
    let shown = match (parts.next(), parts.next(), parts.next()) {
        (Some(_), Some(_), Some(rest)) => rest.to_owned(),
        _ => base.clone(),
    };
    sanitize_display_name(&shown)
}
