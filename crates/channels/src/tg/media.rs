// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The files of a final reply (ADR-0052): each path checked on disk and against the Bot API
//! upload limits, routed by its extension, and sent after the text through the shared
//! [`Sender`] once, without a retry; a refused or failed file gets the reference's one-line
//! notice instead.

use std::fmt;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use crate::bot_api::{self, Album, MediaKind, Sender, Upload};
use crate::render::media::{AUDIO_EXTS, expand_user, normalize};
use crate::render::pytext::path_suffix;
use crate::render::{MediaReply, extract_media, strip_media_for_display};

/// Largest photo `sendPhoto` takes ("10 MB", read as decimal so either reading holds); a
/// larger image goes as a document.
const MAX_PHOTO_BYTES: u64 = 10_000_000;
/// Largest multipart upload of any other kind ("50 MB", read the same way).
const MAX_UPLOAD_BYTES: u64 = 50_000_000;
/// Photos of one `sendMediaGroup`.
const ALBUM_MAX: usize = 10;

const IMAGE_EXTS: &[&str] = &[".jpg", ".jpeg", ".png", ".webp", ".gif"];
const VIDEO_EXTS: &[&str] = &[".mp4", ".mov", ".avi", ".mkv", ".webm", ".3gp"];
const VOICE_EXTS: &[&str] = &[".ogg", ".opus"];
const AUDIO_ATTACHMENT_EXTS: &[&str] = &[".mp3", ".m4a"];

/// System trees no file is delivered from.
const DENIED_PREFIXES: &[&str] = &[
    "/etc", "/proc", "/sys", "/dev", "/root", "/boot", "/var/log", "/var/lib", "/var/run",
];
/// Credential and configuration trees under `$HOME` no file is delivered from.
const DENIED_HOME_SUBPATHS: &[&str] = &[
    ".ssh",
    ".aws",
    ".gnupg",
    ".kube",
    ".docker",
    ".config",
    ".azure",
    ".gcloud",
    "Library/Keychains",
];

/// The path checks and routing of one `ulei-tg` process.
pub(super) struct Media {
    /// `$HOME`, for `~` in a tag and the credential trees.
    home: Option<String>,
}

/// One send of the files of a reply, in order.
pub(super) enum Step {
    /// Two to ten photos as one album; each falls back to its own photo when the album fails.
    Album(Vec<Item>),
    /// One file through one upload method.
    One(MediaKind, Item),
    /// A refused file: only its notice goes out.
    Refused(String),
}

/// A checked file and the notice that replaces it when its upload fails.
pub(super) struct Item {
    path: PathBuf,
    notice: String,
}

/// How the files of a reply went out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Outcome {
    /// Files the reply asked for.
    pub(super) files: usize,
    /// Files Telegram answered with a message id.
    pub(super) sent: usize,
}

enum Refusal {
    Missing,
    Denied,
    Unreadable(io::ErrorKind),
    TooLarge(u64),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => f.write_str("not an existing absolute file"),
            Self::Denied => f.write_str("under a denied path"),
            Self::Unreadable(kind) => write!(f, "unreadable: {kind}"),
            Self::TooLarge(size) => write!(f, "{size} bytes, over {MAX_UPLOAD_BYTES}"),
        }
    }
}

impl Media {
    /// Checks and routing with `$HOME` of this process.
    pub(super) fn from_env() -> Self {
        Self {
            home: std::env::var("HOME").ok(),
        }
    }

    /// `text` split into its `MEDIA:` files and the text to show.
    pub(super) fn extract(&self, text: &str) -> MediaReply {
        extract_media(text, self.home.as_deref(), &|path| self.deliverable(path))
    }

    /// The path to send for `path` when it passes [`Self::resolve`].
    fn deliverable(&self, path: &str) -> Option<String> {
        self.resolve(path)
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    }

    /// `text` of an interim message with its `MEDIA:` tags hidden; no file is sent for it.
    pub(super) fn display(&self, text: &str) -> String {
        strip_media_for_display(text, &|path| self.deliverable(path))
    }

    /// The sends of `reply`'s files: photos in albums of up to ten (one alone as a photo), then
    /// photos over the photo limit as documents and the notices of refused photos, then every
    /// other file in order. Each refusal is logged with the path.
    pub(super) fn plan(&self, chat_id: i64, reply: &MediaReply) -> Vec<Step> {
        let mut photos = Vec::new();
        let mut after_photos = Vec::new();
        let mut rest = Vec::new();
        for file in &reply.files {
            let kind = route(&file.path, file.is_voice, reply.as_document);
            let notice = failure_notice(&file.path, file.is_voice);
            let slot = if kind == MediaKind::Photo {
                &mut after_photos
            } else {
                &mut rest
            };
            match self.check(&file.path) {
                Err(refusal) => {
                    eprintln!(
                        "ulei-tg: MEDIA file {} for chat {chat_id} refused: {refusal}",
                        log_safe(&file.path)
                    );
                    slot.push(Step::Refused(notice));
                }
                Ok((path, size)) if kind == MediaKind::Photo && size > MAX_PHOTO_BYTES => {
                    after_photos.push(Step::One(MediaKind::Document, Item { path, notice }));
                }
                Ok((path, _)) if kind == MediaKind::Photo => photos.push(Item { path, notice }),
                Ok((path, _)) => rest.push(Step::One(kind, Item { path, notice })),
            }
        }
        let mut steps = Vec::new();
        let mut photos = photos.into_iter().peekable();
        while photos.peek().is_some() {
            let mut chunk: Vec<Item> = photos.by_ref().take(ALBUM_MAX).collect();
            steps.push(if chunk.len() == 1 {
                Step::One(MediaKind::Photo, chunk.remove(0))
            } else {
                Step::Album(chunk)
            });
        }
        steps.extend(after_photos);
        steps.extend(rest);
        steps
    }

    /// The reference's `validate_media_delivery_path`: an absolute path after `~` expansion that
    /// resolves to a regular file outside the denied trees.
    fn resolve(&self, raw: &str) -> Result<PathBuf, Refusal> {
        let path = expand_user(&normalize(raw), self.home.as_deref());
        let path = Path::new(&path);
        if !path.is_absolute() {
            return Err(Refusal::Missing);
        }
        let resolved = fs::canonicalize(path).map_err(|_| Refusal::Missing)?;
        if !resolved.is_file() {
            return Err(Refusal::Missing);
        }
        if self.denied(&resolved) {
            return Err(Refusal::Denied);
        }
        Ok(resolved)
    }

    /// [`Self::resolve`], then the file opens for reading and fits the upload limit.
    fn check(&self, raw: &str) -> Result<(PathBuf, u64), Refusal> {
        let path = self.resolve(raw)?;
        let file = File::open(&path).map_err(|e| Refusal::Unreadable(e.kind()))?;
        let size = file
            .metadata()
            .map_err(|e| Refusal::Unreadable(e.kind()))?
            .len();
        if size > MAX_UPLOAD_BYTES {
            return Err(Refusal::TooLarge(size));
        }
        Ok((path, size))
    }

    /// Whether `resolved` lies in a denied tree; `$HOME` itself is never denied as a whole.
    fn denied(&self, resolved: &Path) -> bool {
        let home = self.home.as_deref().map(|h| canonical(Path::new(h)));
        let mut trees: Vec<PathBuf> = DENIED_PREFIXES.iter().map(PathBuf::from).collect();
        if let Some(home) = &self.home {
            trees.extend(
                DENIED_HOME_SUBPATHS
                    .iter()
                    .map(|sub| Path::new(home).join(sub)),
            );
        }
        trees
            .iter()
            .map(|tree| canonical(tree))
            .any(|tree| resolved.starts_with(&tree) && home.as_ref() != Some(&tree))
    }
}

/// Sends `steps` to `chat_id` through `sender`; `notice` delivers the line of a refused file or
/// a failed upload. A photo that fails goes again as a document, an album that fails goes again
/// photo by photo, unless the failure was a response timeout: such an upload may have arrived.
pub(super) fn send(
    sender: &Sender,
    chat_id: i64,
    steps: Vec<Step>,
    notice: &mut dyn FnMut(&str),
) -> Outcome {
    let mut outcome = Outcome::default();
    for step in steps {
        match step {
            Step::Refused(line) => {
                outcome.files += 1;
                notice(&line);
            }
            Step::One(kind, item) => {
                outcome.files += 1;
                if send_one(sender, chat_id, kind, &item, notice) {
                    outcome.sent += 1;
                }
            }
            Step::Album(items) => {
                outcome.files += items.len();
                let album = Album {
                    chat_id,
                    paths: items.iter().map(|item| item.path.clone()).collect(),
                    disable_notification: false,
                };
                match sender.send_media_group(&album) {
                    Ok(_) => outcome.sent += items.len(),
                    Err(err) => {
                        eprintln!(
                            "ulei-tg: album of {} photos to chat {chat_id} failed: {err}",
                            items.len()
                        );
                        for item in &items {
                            if err.is_timeout() {
                                notice(&item.notice);
                            } else if send_one(sender, chat_id, MediaKind::Photo, item, notice) {
                                outcome.sent += 1;
                            }
                        }
                    }
                }
            }
        }
    }
    outcome
}

/// One upload; true when Telegram answered it with a message.
fn send_one(
    sender: &Sender,
    chat_id: i64,
    kind: MediaKind,
    item: &Item,
    notice: &mut dyn FnMut(&str),
) -> bool {
    let Err(err) = sender.send_media(&upload(chat_id, kind, &item.path)) else {
        return true;
    };
    log_failure(chat_id, kind, &item.path, &err);
    if kind == MediaKind::Photo && !err.is_timeout() {
        match sender.send_media(&upload(chat_id, MediaKind::Document, &item.path)) {
            Ok(_) => return true,
            Err(err) => log_failure(chat_id, MediaKind::Document, &item.path, &err),
        }
    }
    notice(&item.notice);
    false
}

fn upload(chat_id: i64, kind: MediaKind, path: &Path) -> Upload {
    Upload {
        kind,
        chat_id,
        path: path.to_owned(),
        file_name: None,
        caption: None,
        parse_mode: None,
        reply_to: None,
        disable_notification: false,
    }
}

fn log_failure(chat_id: i64, kind: MediaKind, path: &Path, err: &bot_api::Error) {
    eprintln!(
        "ulei-tg: MEDIA file {} to chat {chat_id} as {kind:?} failed: {err}",
        log_safe(&path.to_string_lossy())
    );
}

/// The upload method of `path`: images as photos unless `as_document`, OGG/Opus as voice only
/// when `is_voice`, MP3/M4A as audio, videos as videos, everything else as documents.
fn route(path: &str, is_voice: bool, as_document: bool) -> MediaKind {
    let ext = path_suffix(path);
    if IMAGE_EXTS.contains(&ext.as_str()) && !is_voice && !as_document {
        MediaKind::Photo
    } else if sent_as_audio(&ext, is_voice) {
        if VOICE_EXTS.contains(&ext.as_str()) {
            MediaKind::Voice
        } else {
            MediaKind::Audio
        }
    } else if VIDEO_EXTS.contains(&ext.as_str()) {
        MediaKind::Video
    } else {
        MediaKind::Document
    }
}

/// The reference's `should_send_media_as_audio` for Telegram.
fn sent_as_audio(ext: &str, is_voice: bool) -> bool {
    if !AUDIO_EXTS.contains(&ext) {
        return false;
    }
    if VOICE_EXTS.contains(&ext) {
        return is_voice;
    }
    AUDIO_ATTACHMENT_EXTS.contains(&ext)
}

/// The owner's line for a file that did not go out, as `_notify_media_delivery_failure` words it.
fn failure_notice(path: &str, is_voice: bool) -> String {
    let ext = path_suffix(path);
    if is_voice || sent_as_audio(&ext, is_voice) {
        "\u{26a0}\u{fe0f} Couldn't deliver the audio attachment.".to_owned()
    } else if VIDEO_EXTS.contains(&ext.as_str()) {
        "\u{26a0}\u{fe0f} Couldn't deliver the video attachment.".to_owned()
    } else {
        format!(
            "\u{26a0}\u{fe0f} Couldn't deliver the file attachment ({}).",
            base_name(path)
        )
    }
}

/// `os.path.basename`.
fn base_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or_default()
}

/// `path` on one line and at most 200 characters, control and line-separator characters as `?`.
fn log_safe(path: &str) -> String {
    path.chars()
        .take(200)
        .map(|c| {
            if matches!(
                c,
                '\0'..='\x1f' | '\x7f' | '\u{85}' | '\u{2028}' | '\u{2029}'
            ) {
                '?'
            } else {
                c
            }
        })
        .collect()
}

/// `path` resolved, or as given when it does not resolve.
fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
}
