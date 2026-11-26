// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The text the engine sees for a turn: transcripts of voice notes, notes about saved audio,
//! video and documents, and the reply pointer, around the owner's own words, and one pointer per
//! saved image after them; the images themselves go to the engine beside the text.

use std::error::Error;
use std::path::Path;

use super::event::{Event, MessageKind, ReplyContext};
use super::media::{Attachment, display_name_of};
use crate::render::pytext::extension;

/// Speech to text for voice notes.
///
/// Seam: [`crate::stt::Stt`] implements it.
pub trait Transcriber {
    /// Transcript of the audio file at `path`.
    ///
    /// # Errors
    /// The file could not be transcribed.
    fn transcribe(&self, path: &Path) -> Result<String, Box<dyn Error + Send + Sync>>;
}

/// One turn for the engine.
///
/// Seam: C6 `submit_turn` carries it.
#[derive(Debug, Clone, PartialEq)]
pub struct Turn {
    /// Chat to answer in.
    pub chat_id: i64,
    /// Last Telegram message of the turn, which the answer replies to; `None` for an internal
    /// wake-up, whose answer is a plain message.
    pub message_id: Option<i64>,
    /// Update that carried that message.
    pub update_id: i64,
    /// Unix time of the first message of the turn.
    pub date: i64,
    /// What the turn mainly is.
    pub kind: MessageKind,
    /// Text the model sees as the user message.
    pub text: String,
    /// Context appended only to the copy of the user message sent to the model, never stored.
    pub context: Option<String>,
    /// Saved files, each noted or transcribed in `text`.
    pub attachments: Vec<Attachment>,
    /// Non-empty transcripts of its voice notes in order, for the echo to the owner; only the
    /// quoted copies in `text` reach the engine.
    pub transcripts: Vec<String>,
    /// The message the turn replies to.
    pub reply: Option<ReplyContext>,
    /// A system event (not the owner's words): skips coalescing.
    pub internal: bool,
}

impl Turn {
    /// Absolute paths of the turn's images in arrival order, at most [`MAX_IMAGES`], which go
    /// to the engine beside `text`.
    #[must_use]
    pub fn image_paths(&self) -> Vec<String> {
        images(&self.attachments)
            .filter(|a| a.path.is_absolute())
            .take(MAX_IMAGES)
            .map(|a| a.path.display().to_string())
            .collect()
    }
}

/// Most images of one turn the engine takes (C6 `SubmitTurnRequest.images`).
const MAX_IMAGES: usize = 10;

const FORWARD_PREFIX: &str = "[Переслано от ";
const TEXT_EXTENSIONS: &[&str] = &[
    ".txt", ".md", ".csv", ".log", ".json", ".xml", ".yaml", ".yml", ".toml", ".ini", ".cfg",
];
const REPLY_SNIPPET_CHARS: usize = 500;
/// The text of an image turn without words.
const DEFAULT_IMAGE_PROMPT: &str = "What do you see in this image?";

fn mime_is(attachment: &Attachment, prefix: &str) -> bool {
    attachment.mime.starts_with(prefix)
}

/// Builds the turn of a merged event.
pub(crate) fn prepare(event: Event, stt: Option<&dyn Transcriber>) -> Turn {
    let mut text = event.text;
    let mut audio_files: Vec<&Path> = Vec::new();
    let mut stt_inputs: Vec<&Attachment> = Vec::new();
    let mut videos: Vec<&Path> = Vec::new();
    for a in &event.attachments {
        if event.kind == MessageKind::Audio {
            audio_files.push(&a.path);
        } else if !matches!(event.kind, MessageKind::Document)
            && (event.kind == MessageKind::Voice || mime_is(a, "audio/"))
        {
            stt_inputs.push(a);
        }
        if mime_is(a, "video/") {
            videos.push(&a.path);
        }
    }
    let mut transcripts = Vec::new();
    if !stt_inputs.is_empty() {
        text = with_transcripts(&text, &stt_inputs, stt, &mut transcripts);
    }
    for path in audio_files {
        let note = format!(
            "[The user sent an audio file attachment: '{}'. It is saved at: {}. Its content is not \
             inlined here. If the user's request involves what the audio contains, transcribe or \
             process it yourself — for example by passing the path to a transcription or media \
             tool — instead of asking the user to describe it. Only ask what to do with it if \
             their intent is genuinely unclear.]",
            display_name_of(path),
            path.display()
        );
        text = format!("{note}\n\n{text}");
    }
    for path in videos {
        let note = format!(
            "[The user sent a video attachment: '{}'. It is saved at: {}. Its content is not \
             inlined here. If the user's request involves what the video contains, inspect or \
             process it yourself — for example by passing the path to a video analysis or media \
             tool — instead of asking the user to describe it. Only ask what to do with it if \
             their intent is genuinely unclear.]",
            display_name_of(path),
            path.display()
        );
        text = format!("{note}\n\n{text}");
    }
    for a in &event.attachments {
        if mime_is(a, "image/") || mime_is(a, "audio/") || mime_is(a, "video/") {
            continue;
        }
        let note = document_note(&display_name_of(&a.path), &a.path, &document_mime(a));
        text = format!("{note}\n\n{text}");
    }
    if let Some(reply_text) = event
        .reply
        .as_ref()
        .and_then(|r| r.text.as_deref())
        .filter(|t| !t.is_empty())
    {
        let snippet: String = reply_text.chars().take(REPLY_SNIPPET_CHARS).collect();
        text = format!("[Replying to: \"{snippet}\"]\n\n{text}");
    }
    let text = with_image_hints(text, &event.attachments);
    Turn {
        chat_id: event.chat_id,
        message_id: Some(event.message_id),
        update_id: event.update_id,
        date: event.date,
        kind: event.kind,
        text,
        context: None,
        attachments: event.attachments,
        transcripts,
        reply: event.reply,
        internal: false,
    }
}

/// The text of a user turn with images attached natively: the prepared text, or a neutral
/// question when it is empty, then one pointer per image, so tools that take a path can reach the
/// same file. The text unchanged without images.
fn with_image_hints(text: String, attachments: &[Attachment]) -> String {
    let hints: Vec<String> = images(attachments)
        .map(|a| format!("[Image attached at: {}]", a.path.display()))
        .collect();
    if hints.is_empty() {
        return text;
    }
    let base = match text.trim() {
        "" => DEFAULT_IMAGE_PROMPT,
        trimmed => trimmed,
    };
    format!("{base}\n\n{}", hints.join("\n"))
}

/// The turn's images in arrival order: the attachments whose own MIME type is an image.
fn images(attachments: &[Attachment]) -> impl Iterator<Item = &Attachment> {
    attachments.iter().filter(|a| mime_is(a, "image/"))
}

fn document_mime(a: &Attachment) -> String {
    if !a.mime.is_empty() && a.mime != "application/octet-stream" {
        return a.mime.clone();
    }
    let ext = extension(&a.path.to_string_lossy());
    if TEXT_EXTENSIONS.contains(&ext.as_str()) || ext == ".html" || ext == ".htm" || ext == ".py" {
        "text/plain".to_owned()
    } else {
        "application/octet-stream".to_owned()
    }
}

fn document_note(display_name: &str, path: &Path, mime: &str) -> String {
    if mime.starts_with("text/") {
        return format!(
            "[The user sent a text document: '{display_name}'. Its content has been included below. \
             The file is also saved at: {}]",
            path.display()
        );
    }
    format!(
        "[The user sent a document: '{display_name}'. It is saved at: {}. Its text is not inlined \
         here (it's a binary format such as PDF or DOCX). To read it, extract the document's text \
         yourself — for example with the terminal tool or the ocr-and-documents skill — before \
         answering, instead of asking the user to paste the contents.]",
        path.display()
    )
}

/// `M:SS` or `H:MM:SS`.
fn format_duration(total: u64) -> String {
    let (hours, rem) = (total / 3600, total % 3600);
    let (minutes, secs) = (rem / 60, rem % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes}:{secs:02}")
    }
}

/// Prepends transcripts (or the failure notes) to the text, keeping a leading forward label on
/// top so the agent sees whose voice note it is; each non-empty transcript is also pushed to
/// `transcripts`.
fn with_transcripts(
    text: &str,
    inputs: &[&Attachment],
    stt: Option<&dyn Transcriber>,
    transcripts: &mut Vec<String>,
) -> String {
    if text.starts_with(FORWARD_PREFIX) {
        let line_end = text.find('\n').unwrap_or(text.len());
        let label = &text[..line_end];
        if label.ends_with(']') {
            let rest = text[line_end..].trim_start_matches('\n');
            let enriched = with_transcripts(rest, inputs, stt, transcripts);
            return if enriched.is_empty() {
                label.to_owned()
            } else {
                format!("{label}\n{enriched}")
            };
        }
    }
    let mut seen: Vec<&Path> = Vec::new();
    let mut parts: Vec<String> = Vec::new();
    for a in inputs {
        if seen.contains(&a.path.as_path()) {
            continue;
        }
        seen.push(&a.path);
        let path = a.path.display();
        let part = match stt {
            None => match a.duration_secs {
                Some(d) => format!("[The user sent a voice message: {path} (duration: {})]", format_duration(d)),
                None => format!("[The user sent a voice message: {path}]"),
            },
            Some(stt) => match stt.transcribe(&a.path) {
                Ok(t) if t.trim().is_empty() => "[The user sent a voice message but it came through empty or \
                     inaudible — speech-to-text returned no words. Do not guess at the content; ask \
                     the user to resend or type it out.]"
                    .to_owned(),
                Ok(t) => {
                    let quoted = format!("\"{t}\"");
                    transcripts.push(t);
                    quoted
                }
                Err(e) => {
                    eprintln!("ulei-tg: voice transcription failed for {path}: {e}");
                    format!(
                        "[voice message could not be transcribed automatically; the audio is available at: {path}]"
                    )
                }
            },
        };
        parts.push(part);
    }
    let prefix = parts.join("\n\n");
    if text.is_empty() {
        prefix
    } else {
        format!("{prefix}\n\n{text}")
    }
}
