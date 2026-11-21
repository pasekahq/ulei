// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The ingress: owner allowlist, one handler per message kind, attachments, reactions, and the
//! coalescer, behind three calls: [`Ingress::accept`], [`Ingress::poll`] and
//! [`Ingress::next_deadline`].

use std::path::PathBuf;
use std::time::Instant;

use super::coalesce::{Coalescer, Timing};
use super::command::Command;
use super::event::{Event, MessageKind, ReplyContext};
use super::forward::forward_label;
use super::localtime::Zone;
use super::media::{
    self, AUDIO_TYPES, Attachment, AttachmentKind, DOCUMENT_TYPES, FetchError, Fetched, FileSource,
    IMAGE_EXTENSIONS, Inbox, TEXT_INJECT_EXTENSIONS, VIDEO_TYPES,
};
use super::prepare::{Transcriber, Turn, prepare};
use super::reaction::{
    Author, MessageTexts, ReactionConfig, ReactionRecord, UNSEEN_SHOWN, context_block,
};
use super::text::{
    clean_bot_trigger_text, clean_command_text, custom_emoji_text, message_plain_text,
    rich_message_text, rich_reply_text, sanitize_display_name,
};
use super::update::{FileRef, Message, MessageReactionUpdated, Update, UpdateKind};
use crate::render::pytext::extension;

/// File size cap of the public Bot API (`getFile` serves up to 20 MB).
pub const PUBLIC_API_MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;
/// File size cap with a local Bot API server.
pub const LOCAL_API_MAX_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Cap of one image, audio or video download.
pub const DEFAULT_MAX_MEDIA_BYTES: u64 = 128 * 1024 * 1024;
const MAX_TEXT_INJECT_BYTES: u64 = 100 * 1024;

/// Settings of the ingress.
#[derive(Debug, Clone)]
pub struct IngressConfig {
    /// Telegram user ids of the owner; everyone else is dropped.
    pub owner_ids: Vec<i64>,
    /// Username of the bot, removed from `@bot` mentions in the text.
    pub bot_username: Option<String>,
    /// Directory attachments are saved to (`<artifacts>/inbox`).
    pub inbox_dir: PathBuf,
    /// Zone of the dates shown to the agent.
    pub zone: Zone,
    /// Cap of a document (and of any file declared larger).
    pub max_file_bytes: u64,
    /// Cap of one image, audio or video download.
    pub max_media_bytes: u64,
    /// Batching and coalescing delays.
    pub timing: Timing,
    /// Reaction wake-up.
    pub reactions: ReactionConfig,
}

/// I/O the ingress needs from other pieces.
pub struct Ports {
    /// Telegram file downloads.
    pub files: Box<dyn FileSource>,
    /// Speech to text; without it voice notes reach the agent as file and note.
    pub stt: Option<Box<dyn Transcriber>>,
    /// Texts of earlier messages, for reactions and replies to rich messages.
    pub texts: Option<Box<dyn MessageTexts>>,
}

/// What the daemon does next.
#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    /// Submit a turn to the engine.
    Turn(Turn),
    /// Run an owner command and send its reply.
    Command {
        /// Chat to reply in.
        chat_id: i64,
        /// The command message.
        message_id: i64,
        /// The command.
        command: Command,
    },
    /// Tell the owner something at once (a failed download).
    Notice {
        /// Chat to reply in.
        chat_id: i64,
        /// Message to reply to.
        reply_to: i64,
        /// Text.
        text: String,
    },
}

/// Counters of what was not turned into work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    /// Messages and reactions from someone other than the owner, dropped silently.
    pub strangers: u64,
    /// Messages outside private chats.
    pub other_chats: u64,
    /// Updates of a kind the ingress does not handle.
    pub skipped_updates: u64,
    /// Owner messages no handler understands (logged by the reference, not delivered).
    pub unhandled: u64,
}

/// The inbound half of the Telegram channel.
pub struct Ingress {
    config: IngressConfig,
    ports: Ports,
    inbox: Inbox,
    coalescer: Coalescer,
    unseen: Vec<ReactionRecord>,
    wake: Option<(Instant, i64)>,
    stats: Stats,
}

impl Ingress {
    /// A new ingress with nothing pending.
    pub fn new(config: IngressConfig, ports: Ports) -> Self {
        Self {
            inbox: Inbox::new(config.inbox_dir.clone()),
            coalescer: Coalescer::new(config.timing.clone()),
            config,
            ports,
            unseen: Vec::new(),
            wake: None,
            stats: Stats::default(),
        }
    }

    /// Counters since start.
    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// When [`Ingress::poll`] next has work; `None` when idle.
    pub fn next_deadline(&self) -> Option<Instant> {
        let wake = self.wake.map(|(at, _)| at);
        match (self.coalescer.next_deadline(), wake) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Takes one update received at `now`; returns work that is due at once.
    pub fn accept(&mut self, update: &Update, now: Instant) -> Vec<Output> {
        let mut out = Vec::new();
        match update.kind() {
            Some(UpdateKind::Message { message, edited }) => {
                if message.chat.kind != "private" {
                    self.stats.other_chats += 1;
                } else if !message
                    .from_user
                    .as_ref()
                    .is_some_and(|u| self.config.owner_ids.contains(&u.id))
                {
                    self.stats.strangers += 1;
                } else {
                    self.on_message(message, update.update_id, edited, now, &mut out);
                }
            }
            Some(UpdateKind::Reaction(reaction)) => self.on_reaction(reaction, now),
            None => self.stats.skipped_updates += 1,
        }
        out
    }

    /// Runs the timers due by `now`; returns the turns to submit, in order.
    pub fn poll(&mut self, now: Instant) -> Vec<Output> {
        let events = self.coalescer.poll(now);
        let mut out: Vec<Output> = events
            .into_iter()
            .map(|e| Output::Turn(self.turn(e)))
            .collect();
        if let Some((_, chat_id)) = self.wake.filter(|(at, _)| *at <= now) {
            self.wake = None;
            // Reactions an owner turn already showed do not wake the agent again.
            if let Some(context) =
                context_block(&self.config.reactions.context_header, &self.unseen)
            {
                self.unseen.clear();
                out.push(Output::Turn(Turn {
                    chat_id,
                    message_id: None,
                    update_id: 0,
                    date: 0,
                    kind: MessageKind::Text,
                    text: self.config.reactions.wake_text.clone(),
                    context: Some(context),
                    attachments: Vec::new(),
                    transcripts: Vec::new(),
                    reply: None,
                    internal: true,
                }));
            }
        }
        out
    }

    fn turn(&mut self, event: Event) -> Turn {
        let mut turn = prepare(event, self.ports.stt.as_deref());
        turn.context = context_block(&self.config.reactions.context_header, &self.unseen);
        self.unseen.clear();
        turn
    }

    fn on_reaction(&mut self, reaction: &MessageReactionUpdated, now: Instant) {
        // Only the owner's private chat is served; a reaction elsewhere must not wake the agent
        // into a group.
        if reaction.chat.kind != "private" {
            self.stats.other_chats += 1;
            return;
        }
        let actor = reaction
            .user
            .as_ref()
            .map(|u| u.id)
            .or_else(|| reaction.actor_chat.as_ref().map(|c| c.id));
        if !actor.is_some_and(|id| self.config.owner_ids.contains(&id)) {
            self.stats.strangers += 1;
            return;
        }
        let ts = self.config.zone.local(reaction.date).iso();
        let text = self
            .ports
            .texts
            .as_ref()
            .and_then(|t| t.text_of(reaction.chat.id, reaction.message_id));
        let record = ReactionRecord::new(reaction, ts, text);
        if record.wakes() && self.wake.is_none() {
            self.wake = Some((now + self.config.reactions.wake_delay, record.chat_id));
        }
        self.unseen.push(record);
        // Only the last ten are ever shown (`context_block`); removals never wake the agent, so
        // without this cap they would pile up.
        if self.unseen.len() > UNSEEN_SHOWN {
            self.unseen.drain(..self.unseen.len() - UNSEEN_SHOWN);
        }
    }

    fn on_message(
        &mut self,
        msg: &Message,
        update_id: i64,
        edited: bool,
        now: Instant,
        out: &mut Vec<Output>,
    ) {
        let has_text = msg.text.as_deref().is_some_and(|t| !t.is_empty());
        let is_command = has_text
            && msg
                .entities
                .first()
                .is_some_and(|e| e.kind == "bot_command" && e.offset == 0);
        if is_command {
            self.on_command(msg, update_id, out);
        } else if has_text
            || (msg.text.is_none()
                && msg
                    .rich_message
                    .as_ref()
                    .is_some_and(|r| !rich_message_text(r).is_empty()))
        {
            let mut event = self.build(msg, MessageKind::Text, update_id);
            event.text = clean_bot_trigger_text(&event.text, self.config.bot_username.as_deref());
            self.apply_forward_label(msg, &mut event);
            self.cache_replied_media(msg, &mut event);
            self.coalescer.push_text(event, now);
        } else if msg.location.is_some() || msg.venue.is_some() {
            self.on_location(msg, update_id, now, out);
        } else if !edited && has_media(msg) {
            self.on_media(msg, update_id, now, out);
        } else {
            self.on_unhandled(msg, update_id, now);
        }
    }

    fn build(&self, msg: &Message, kind: MessageKind, update_id: i64) -> Event {
        Event {
            chat_id: msg.chat.id,
            message_id: msg.message_id,
            update_id,
            date: msg.date,
            kind,
            text: message_plain_text(msg),
            attachments: Vec::new(),
            reply: self.reply_context(msg),
            forwarded: false,
            chunks: None,
            last_chunk_len: 0,
        }
    }

    fn reply_context(&self, msg: &Message) -> Option<ReplyContext> {
        let reply = msg.reply_to_message.as_deref()?;
        let text = if let Some(quote) = msg.quote.as_ref().filter(|q| !q.text.is_empty()) {
            Some(custom_emoji_text(&quote.text, &quote.entities))
        } else {
            Some(message_plain_text(reply))
                .filter(|t| !t.is_empty())
                .or_else(|| reply.rich_message.as_ref().and_then(rich_reply_text))
                .or_else(|| {
                    let texts = self.ports.texts.as_ref()?;
                    texts
                        .text_of(msg.chat.id, reply.message_id)
                        .filter(|(_, a)| *a == Author::Bot)
                        .map(|(t, _)| t)
                })
        };
        Some(ReplyContext {
            message_id: reply.message_id,
            text,
        })
    }

    fn apply_forward_label(&self, msg: &Message, event: &mut Event) {
        if let Some(label) = forward_label(msg, &self.config.zone) {
            event.forwarded = true;
            event.text = if event.text.is_empty() {
                label
            } else {
                format!("{label}\n{}", event.text)
            };
        }
    }

    fn on_command(&mut self, msg: &Message, update_id: i64, out: &mut Vec<Output>) {
        let command = Command::parse(&message_plain_text(msg));
        let mut event = self.build(msg, MessageKind::Command, update_id);
        event.text = clean_command_text(&event.text, self.config.bot_username.as_deref());
        // A pending coalesced turn goes out before the command, keeping the order.
        if let Some(pending) = self.coalescer.flush_turn_now(event.chat_id) {
            out.push(Output::Turn(self.turn(pending)));
        }
        match command {
            Some(command) => out.push(Output::Command {
                chat_id: event.chat_id,
                message_id: event.message_id,
                command,
            }),
            // Other slash commands (skills) are the engine's to resolve.
            None => out.push(Output::Turn(self.turn(event))),
        }
    }

    fn on_location(&mut self, msg: &Message, update_id: i64, now: Instant, out: &mut Vec<Output>) {
        let venue = msg.venue.as_ref();
        let Some(location) = venue.map(|v| &v.location).or(msg.location.as_ref()) else {
            return;
        };
        let mut parts = vec!["[The user shared a location pin.]".to_owned()];
        if let Some(venue) = venue {
            if let Some(title) = venue.title.as_deref().filter(|t| !t.is_empty()) {
                parts.push(format!("Venue: {title}"));
            }
            if let Some(address) = venue.address.as_deref().filter(|a| !a.is_empty()) {
                parts.push(format!("Address: {address}"));
            }
        }
        let (lat, lon) = (
            float_repr(location.latitude),
            float_repr(location.longitude),
        );
        parts.push(format!("latitude: {lat}"));
        parts.push(format!("longitude: {lon}"));
        parts.push(format!(
            "Map: https://www.google.com/maps/search/?api=1&query={lat},{lon}"
        ));
        parts.push(
            "Ask what they'd like to find nearby (restaurants, cafes, etc.) and any preferences."
                .to_owned(),
        );
        let mut event = self.build(msg, MessageKind::Location, update_id);
        event.text = parts.join("\n");
        self.dispatch(event, now, out);
    }

    fn dispatch(&mut self, event: Event, now: Instant, out: &mut Vec<Output>) {
        for due in self.coalescer.dispatch(event, now) {
            out.push(Output::Turn(self.turn(due)));
        }
    }

    fn on_unhandled(&mut self, msg: &Message, update_id: i64, now: Instant) {
        let unknown: Vec<&str> = msg
            .other
            .keys()
            .map(String::as_str)
            .filter(|k| !KNOWN_MESSAGE_FIELDS.contains(k))
            .collect();
        if unknown.is_empty() {
            self.stats.unhandled += 1;
            return;
        }
        let mut event = self.build(msg, MessageKind::Text, update_id);
        if event.text.trim().is_empty() {
            event.text = format!(
                "[Telegram: пришло сообщение неподдерживаемого формата (поля: {}); текст извлечь не \
                 удалось — попроси владельца переслать его обычным текстом]",
                unknown.join(", ")
            );
        }
        self.coalescer.push_text(event, now);
    }

    fn surface_failure(
        event: &mut Event,
        kind: &str,
        name: Option<&str>,
        err: &FetchError,
        out: &mut Vec<Output>,
    ) {
        let class = err.class();
        out.push(Output::Notice {
            chat_id: event.chat_id,
            reply_to: event.message_id,
            text: media::cache_failure_reply(kind, name, &class),
        });
        append_note(
            &mut event.text,
            &media::cache_failure_note(kind, name, &class),
        );
    }

    fn media_cap(&self) -> u64 {
        self.config.max_file_bytes.min(self.config.max_media_bytes)
    }

    fn fetch(&mut self, file_id: &str, max_bytes: u64) -> Result<Fetched, FetchError> {
        self.inbox
            .fetch(self.ports.files.as_ref(), file_id, max_bytes)
    }

    fn on_media(&mut self, msg: &Message, update_id: i64, now: Instant, out: &mut Vec<Output>) {
        let kind = if msg.sticker.is_some() {
            MessageKind::Sticker
        } else if !msg.photo.is_empty() {
            MessageKind::Photo
        } else if msg.video.is_some() {
            MessageKind::Video
        } else if msg.audio.is_some() {
            MessageKind::Audio
        } else if msg.voice.is_some() {
            MessageKind::Voice
        } else {
            MessageKind::Document
        };
        let mut event = self.build(msg, kind, update_id);
        if msg.caption.is_some() {
            event.text = clean_bot_trigger_text(&event.text, self.config.bot_username.as_deref());
        }
        if let Some(sticker) = &msg.sticker {
            event.text = sticker_text(
                sticker.emoji.as_deref().unwrap_or_default(),
                sticker.set_name.as_deref().unwrap_or_default(),
                sticker.is_animated || sticker.is_video,
            );
            self.apply_forward_label(msg, &mut event);
            self.dispatch(event, now, out);
            return;
        }
        self.apply_forward_label(msg, &mut event);
        let group = msg.media_group_id.clone();
        if let Some(photo) = msg.photo.last() {
            match self.store_photo(&photo.file_id) {
                Ok(attachment) => {
                    event.attachments.push(attachment);
                    match group {
                        Some(g) => self.coalescer.push_group(g, event, now),
                        None => self.coalescer.push_photo(event, now),
                    }
                    return;
                }
                Err(e) => Self::surface_failure(&mut event, "photo", None, &e, out),
            }
        } else if let Some((file, label, fallback, mime, prefix)) = av_source(msg) {
            if let Some(size) = file.file_size.filter(|s| *s > self.config.max_file_bytes) {
                append_note(
                    &mut event.text,
                    &media::too_large_note(label, size, self.config.max_file_bytes),
                );
                self.dispatch(event, now, out);
                return;
            }
            match self.store_av(&file.file_id, prefix, fallback, mime) {
                Ok(mut attachment) => {
                    attachment.duration_secs = file.duration;
                    event.attachments.push(attachment);
                }
                Err(e) => Self::surface_failure(&mut event, label, None, &e, out),
            }
        } else if let Some(doc) = &msg.document
            && !self.on_document(doc, &mut event, group.as_ref(), now, out)
        {
            return;
        }
        match group {
            Some(g) => self.coalescer.push_group(g, event, now),
            None => self.dispatch(event, now, out),
        }
    }

    fn store_photo(&mut self, file_id: &str) -> Result<Attachment, FetchError> {
        let fetched = self.fetch(file_id, self.media_cap())?;
        if !media::looks_like_image(&media::head(&fetched)) {
            Inbox::discard(&fetched);
            return Err(FetchError::NotAnImage);
        }
        let path_lower = fetched.file_path.to_lowercase();
        let ext = [".png", ".webp", ".gif", ".jpeg", ".jpg"]
            .into_iter()
            .find(|e| path_lower.ends_with(e))
            .unwrap_or(".jpg");
        let path = self.inbox.keep(&fetched, "img", ext)?;
        Ok(Attachment {
            path,
            kind: AttachmentKind::Image,
            mime: format!("image/{}", &ext[1..]),
            size: fetched.size,
            duration_secs: None,
        })
    }

    fn store_av(
        &mut self,
        file_id: &str,
        prefix: &str,
        fallback: &str,
        mime: &str,
    ) -> Result<Attachment, FetchError> {
        let fetched = self.fetch(file_id, self.media_cap())?;
        let (kind, ext, mime) = if prefix == "video" {
            let lower = fetched.file_path.to_lowercase();
            let (ext, mime) = VIDEO_TYPES
                .iter()
                .find(|(e, _)| lower.ends_with(e))
                .copied()
                .unwrap_or((".mp4", "video/mp4"));
            (AttachmentKind::Video, ext, mime)
        } else {
            (
                AttachmentKind::Audio,
                media::sniff_audio_ext(&media::head(&fetched), fallback),
                mime,
            )
        };
        let path = self.inbox.keep(&fetched, prefix, ext)?;
        Ok(Attachment {
            path,
            kind,
            mime: mime.to_owned(),
            size: fetched.size,
            duration_secs: None,
        })
    }

    /// Handles a document; `false` when the event was already sent on.
    fn on_document(
        &mut self,
        doc: &FileRef,
        event: &mut Event,
        group: Option<&String>,
        now: Instant,
        out: &mut Vec<Output>,
    ) -> bool {
        let name = doc.file_name.clone().unwrap_or_default();
        let mut ext = extension(&name);
        let mime = doc.mime_type.as_deref().unwrap_or_default().to_lowercase();
        if ext.is_empty() && !mime.is_empty() {
            media::image_mime_to_ext(&mime)
                .or_else(|| media::reverse_lookup(DOCUMENT_TYPES, &mime))
                .unwrap_or_default()
                .clone_into(&mut ext);
        }
        if doc
            .file_size
            .is_none_or(|s| s == 0 || s > self.config.max_file_bytes)
        {
            let limit_mb = self.config.max_file_bytes / (1024 * 1024);
            event.text = format!(
                "The document is too large or its size could not be verified. Maximum: {limit_mb} MB."
            );
            self.dispatch(event.clone(), now, out);
            return false;
        }
        if IMAGE_EXTENSIONS.contains(&ext.as_str()) || mime.starts_with("image/") {
            return self.on_image_document(doc, event, (&ext, &mime), group, now, out);
        }
        if ext.is_empty() && !mime.is_empty() {
            media::reverse_lookup(VIDEO_TYPES, &mime)
                .unwrap_or_default()
                .clone_into(&mut ext);
        }
        if let Some(video_mime) = media::lookup(VIDEO_TYPES, &ext) {
            let stored = self
                .fetch(&doc.file_id, self.media_cap())
                .and_then(|f| Ok((self.inbox.keep(&f, "video", &ext)?, f.size)));
            match stored {
                Ok((path, size)) => {
                    event.kind = MessageKind::Video;
                    let mime = video_mime.to_owned();
                    let kind = AttachmentKind::Video;
                    event.attachments.push(Attachment {
                        path,
                        kind,
                        mime,
                        size,
                        duration_secs: None,
                    });
                    self.dispatch(event.clone(), now, out);
                    return false;
                }
                Err(e) => {
                    Self::surface_failure(event, "attachment", doc.file_name.as_deref(), &e, out);
                }
            }
            return true;
        }
        match self.store_document(doc, &ext, &mime) {
            Ok((attachment, content)) => {
                if attachment.kind == AttachmentKind::Audio {
                    event.kind = MessageKind::Audio;
                }
                if let Some(content) = content {
                    let display = if name.is_empty() {
                        format!("document{}", if ext.is_empty() { ".txt" } else { &ext })
                    } else {
                        name
                    };
                    let injection = format!(
                        "[Content of {}]:\n{content}",
                        sanitize_display_name(&display)
                    );
                    event.text = if event.text.is_empty() {
                        injection
                    } else {
                        format!("{injection}\n\n{}", event.text)
                    };
                }
                event.attachments.push(attachment);
            }
            Err(e) => Self::surface_failure(event, "attachment", doc.file_name.as_deref(), &e, out),
        }
        true
    }

    /// A document that is really a picture goes the photo way; `false` when sent on.
    fn on_image_document(
        &mut self,
        doc: &FileRef,
        event: &mut Event,
        (ext, mime): (&str, &str),
        group: Option<&String>,
        now: Instant,
        out: &mut Vec<Output>,
    ) -> bool {
        let image_ext = if IMAGE_EXTENSIONS.contains(&ext) {
            ext
        } else {
            media::image_mime_to_ext(mime).unwrap_or(".jpg")
        };
        let fetched = match self.fetch(&doc.file_id, self.media_cap()) {
            Ok(fetched) => fetched,
            Err(e) => {
                Self::surface_failure(event, "attachment", doc.file_name.as_deref(), &e, out);
                return true;
            }
        };
        if !media::looks_like_image(&media::head(&fetched)) {
            Inbox::discard(&fetched);
            let shown = [doc.file_name.as_deref().unwrap_or_default(), mime, ext]
                .into_iter()
                .find(|s| !s.is_empty())
                .unwrap_or("unknown");
            event.text = format!("Image document '{shown}' could not be read as an image.");
            self.dispatch(event.clone(), now, out);
            return false;
        }
        let path = match self.inbox.keep(&fetched, "img", image_ext) {
            Ok(path) => path,
            Err(e) => {
                Self::surface_failure(
                    event,
                    "attachment",
                    doc.file_name.as_deref(),
                    &e.into(),
                    out,
                );
                return true;
            }
        };
        let mime = if mime.starts_with("image/") {
            mime.to_owned()
        } else {
            media::image_ext_to_mime(image_ext).to_owned()
        };
        event.kind = MessageKind::Photo;
        let (kind, size) = (AttachmentKind::Image, fetched.size);
        event.attachments.push(Attachment {
            path,
            kind,
            mime,
            size,
            duration_secs: None,
        });
        match group {
            Some(g) => self.coalescer.push_group(g.clone(), event.clone(), now),
            None => self.coalescer.push_photo(event.clone(), now),
        }
        false
    }

    /// Saves any other file; returns it with its text when that is small UTF-8 text.
    fn store_document(
        &mut self,
        doc: &FileRef,
        ext: &str,
        mime: &str,
    ) -> Result<(Attachment, Option<String>), FetchError> {
        let filename = match doc.file_name.as_deref().filter(|n| !n.is_empty()) {
            Some(name) => name.to_owned(),
            None => format!("document{}", if ext.is_empty() { ".bin" } else { ext }),
        };
        let fetched = self.fetch(&doc.file_id, self.config.max_file_bytes)?;
        let (attachment, _) = self.cache_media(&fetched, &filename, mime, None)?;
        let is_text = TEXT_INJECT_EXTENSIONS.contains(&ext) || mime.starts_with("text/");
        let content = is_text
            .then(|| media::read_small(&attachment.path, MAX_TEXT_INJECT_BYTES))
            .flatten()
            .and_then(|c| String::from_utf8(c).ok());
        Ok((attachment, content))
    }

    /// Classifies and keeps a fetched file; returns it with the name shown to the model. On
    /// failure the fetched file is gone.
    fn cache_media(
        &mut self,
        fetched: &Fetched,
        filename: &str,
        mime: &str,
        default_kind: Option<AttachmentKind>,
    ) -> Result<(Attachment, String), FetchError> {
        let mut ext = extension(filename);
        if ext.is_empty() && !mime.is_empty() {
            [
                media::reverse_lookup(&IMAGE_DOC_TYPES, mime),
                media::reverse_lookup(VIDEO_TYPES, mime),
                media::reverse_lookup(DOCUMENT_TYPES, mime),
            ]
            .into_iter()
            .flatten()
            .next()
            .unwrap_or_default()
            .clone_into(&mut ext);
        }
        let display = if filename.is_empty() {
            let e = ext.trim_start_matches('.');
            if e.is_empty() {
                "file".to_owned()
            } else {
                e.to_owned()
            }
        } else {
            sanitize_display_name(filename)
        };
        let size = fetched.size;
        let attachment = |path, kind, mime: &str| Attachment {
            path,
            kind,
            mime: mime.to_owned(),
            size,
            duration_secs: None,
        };
        if mime.starts_with("image/")
            || IMAGE_EXTENSIONS.contains(&ext.as_str())
            || default_kind == Some(AttachmentKind::Image)
        {
            if !media::looks_like_image(&media::head(fetched)) {
                Inbox::discard(fetched);
                return Err(FetchError::NotAnImage);
            }
            let img_ext = if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
                ext.as_str()
            } else {
                ".jpg"
            };
            let out_mime = if mime.starts_with("image/") {
                mime.to_owned()
            } else {
                media::image_ext_to_mime(img_ext).to_owned()
            };
            let path = self.inbox.keep(fetched, "img", img_ext)?;
            return Ok((attachment(path, AttachmentKind::Image, &out_mime), display));
        }
        if mime.starts_with("video/")
            || media::lookup(VIDEO_TYPES, &ext).is_some()
            || default_kind == Some(AttachmentKind::Video)
        {
            let (vid_ext, vid_mime) = VIDEO_TYPES
                .iter()
                .find(|(e, _)| *e == ext)
                .copied()
                .unwrap_or((".mp4", "video/mp4"));
            let path = self.inbox.keep(fetched, "video", vid_ext)?;
            return Ok((attachment(path, AttachmentKind::Video, vid_mime), display));
        }
        if mime.starts_with("audio/")
            || media::lookup(AUDIO_TYPES, &ext).is_some()
            || default_kind == Some(AttachmentKind::Audio)
        {
            let aud_ext = if media::lookup(AUDIO_TYPES, &ext).is_some() {
                ext.as_str()
            } else {
                ".ogg"
            };
            let aud_ext = media::sniff_audio_ext(&media::head(fetched), aud_ext);
            let out_mime = if mime.starts_with("audio/") {
                mime
            } else {
                media::lookup(AUDIO_TYPES, aud_ext).unwrap_or("audio/ogg")
            };
            let path = self.inbox.keep(fetched, "audio", aud_ext)?;
            return Ok((attachment(path, AttachmentKind::Audio, out_mime), display));
        }
        let fallback = if filename.is_empty() {
            format!("document{}", if ext.is_empty() { ".bin" } else { &ext })
        } else {
            filename.to_owned()
        };
        let tail = format!("_{}", media::safe_file_name(&fallback));
        let path = self.inbox.keep(fetched, "doc", &tail)?;
        let out_mime = media::lookup(DOCUMENT_TYPES, &ext).unwrap_or(if mime.is_empty() {
            "application/octet-stream"
        } else {
            mime
        });
        Ok((
            attachment(path, AttachmentKind::Document, out_mime),
            display,
        ))
    }

    /// Saves the media of the replied-to message and notes where it is.
    fn cache_replied_media(&mut self, msg: &Message, event: &mut Event) {
        let Some(reply) = msg.reply_to_message.as_deref() else {
            return;
        };
        let (file, filename, mime, default_kind) = if let Some(p) = reply.photo.last() {
            (p, String::new(), String::new(), Some(AttachmentKind::Image))
        } else if let Some(v) = &reply.video {
            (
                v,
                String::new(),
                "video/mp4".to_owned(),
                Some(AttachmentKind::Video),
            )
        } else if let Some(v) = &reply.voice {
            (
                v,
                "voice.ogg".to_owned(),
                "audio/ogg".to_owned(),
                Some(AttachmentKind::Audio),
            )
        } else if let Some(a) = &reply.audio {
            (
                a,
                a.file_name.clone().unwrap_or_default(),
                String::new(),
                Some(AttachmentKind::Audio),
            )
        } else if let Some(d) = &reply.document {
            (
                d,
                d.file_name.clone().unwrap_or_default(),
                d.mime_type.as_deref().unwrap_or_default().to_lowercase(),
                None,
            )
        } else {
            return;
        };
        if !file
            .file_size
            .is_some_and(|s| s > 0 && s <= self.config.max_file_bytes)
        {
            return;
        }
        let Ok(fetched) = self.fetch(&file.file_id, self.config.max_file_bytes) else {
            return;
        };
        let filename = if filename.is_empty() {
            fetched
                .file_path
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_owned()
        } else {
            filename
        };
        let Ok((attachment, display)) = self.cache_media(&fetched, &filename, &mime, default_kind)
        else {
            return;
        };
        let (word, kind) = match attachment.kind {
            AttachmentKind::Image => ("image", Some(MessageKind::Photo)),
            AttachmentKind::Video => ("video", Some(MessageKind::Video)),
            AttachmentKind::Audio => ("audio", Some(MessageKind::Audio)),
            AttachmentKind::Document => ("document", None),
        };
        let note = format!(
            "[Replied-to {word} '{display}' saved at: {}]",
            attachment.path.display()
        );
        event.attachments.push(attachment);
        if event.attachments.len() == 1
            && let Some(kind) = kind
        {
            event.kind = kind;
        }
        append_note(&mut event.text, &note);
    }
}

const IMAGE_DOC_TYPES: [(&str, &str); 5] = [
    (".jpg", "image/jpeg"),
    (".jpeg", "image/jpeg"),
    (".png", "image/png"),
    (".webp", "image/webp"),
    (".gif", "image/gif"),
];

fn has_media(msg: &Message) -> bool {
    !msg.photo.is_empty()
        || msg.video.is_some()
        || msg.audio.is_some()
        || msg.voice.is_some()
        || msg.document.is_some()
        || msg.sticker.is_some()
}

/// Voice, audio or video: the file, its label in notes, fallback extension, MIME and name prefix.
fn av_source(
    msg: &Message,
) -> Option<(
    &FileRef,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
)> {
    if let Some(v) = &msg.voice {
        return Some((v, "voice message", ".ogg", "audio/ogg", "audio"));
    }
    if let Some(a) = &msg.audio {
        return Some((a, "audio file", ".mp3", "audio/mp3", "audio"));
    }
    msg.video
        .as_ref()
        .map(|v| (v, "video file", ".mp4", "video/mp4", "video"))
}

fn append_note(text: &mut String, note: &str) {
    if note.is_empty() {
        return;
    }
    *text = if text.is_empty() {
        note.to_owned()
    } else {
        format!("{text}\n\n{note}")
    };
}

fn sticker_text(emoji: &str, set_name: &str, animated: bool) -> String {
    if animated {
        return if emoji.is_empty() {
            "[The user sent an animated sticker~ I can't see animated ones yet]".to_owned()
        } else {
            format!(
                "[The user sent an animated sticker {emoji}~ I can't see animated ones yet, but the emoji suggests: {emoji}]"
            )
        };
    }
    // Static stickers are described by a vision call in the reference; until the engine offers
    // one, the reference's own fallback for a failed description is used.
    let description = if emoji.is_empty() {
        "a sticker".to_owned()
    } else {
        format!("a sticker with emoji {emoji}")
    };
    let context = match (set_name.is_empty(), emoji.is_empty()) {
        (false, false) => format!(" {emoji} from \"{set_name}\""),
        (_, false) => format!(" {emoji}"),
        _ => String::new(),
    };
    format!("[The user sent a sticker{context}~ It shows: \"{description}\" (=^.w.^=)]")
}

/// A float as the reference prints it (`52.0`, not `52`).
fn float_repr(v: f64) -> String {
    if v.fract() == 0.0 && v.is_finite() {
        format!("{v:.1}")
    } else {
        v.to_string()
    }
}

/// Bot API `Message` fields the reference's client library knows but no handler here reads;
/// a message carrying only these is logged, any other field makes it a placeholder turn.
const KNOWN_MESSAGE_FIELDS: &[&str] = &[
    "message_thread_id",
    "direct_messages_topic",
    "sender_chat",
    "sender_boost_count",
    "sender_business_bot",
    "business_connection_id",
    "is_topic_message",
    "is_automatic_forward",
    "external_reply",
    "reply_to_story",
    "reply_to_checklist_task_id",
    "via_bot",
    "edit_date",
    "has_protected_content",
    "is_paid_post",
    "is_from_offline",
    "author_signature",
    "paid_star_count",
    "link_preview_options",
    "suggested_post_info",
    "effect_id",
    "animation",
    "paid_media",
    "story",
    "show_caption_above_media",
    "has_media_spoiler",
    "checklist",
    "dice",
    "game",
    "poll",
    "new_chat_members",
    "left_chat_member",
    "new_chat_title",
    "new_chat_photo",
    "delete_chat_photo",
    "group_chat_created",
    "supergroup_chat_created",
    "channel_chat_created",
    "message_auto_delete_timer_changed",
    "migrate_to_chat_id",
    "migrate_from_chat_id",
    "pinned_message",
    "invoice",
    "successful_payment",
    "refunded_payment",
    "users_shared",
    "chat_shared",
    "gift",
    "unique_gift",
    "connected_website",
    "write_access_allowed",
    "passport_data",
    "proximity_alert_triggered",
    "boost_added",
    "chat_background_set",
    "checklist_tasks_done",
    "checklist_tasks_added",
    "direct_message_price_changed",
    "forum_topic_created",
    "forum_topic_edited",
    "forum_topic_closed",
    "forum_topic_reopened",
    "general_forum_topic_hidden",
    "general_forum_topic_unhidden",
    "giveaway_created",
    "giveaway",
    "giveaway_winners",
    "giveaway_completed",
    "paid_message_price_changed",
    "suggested_post_approved",
    "suggested_post_approval_failed",
    "suggested_post_declined",
    "suggested_post_paid",
    "suggested_post_refunded",
    "video_chat_scheduled",
    "video_chat_started",
    "video_chat_ended",
    "video_chat_participants_invited",
    "web_app_data",
    "reply_markup",
    "forward_from_message_id",
];
