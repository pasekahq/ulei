//! Telegram update -> inbound turns for the engine.
//!
//! [`Ingress`] takes raw Bot API updates ([`Update`]), drops everyone but the owner, turns each
//! message into the text the agent sees (forward labels, reply context, attachments saved to the
//! inbox, transcripts of voice notes), joins rapid messages into one [`Turn`] and parses owner
//! [`Command`]s, which [`run_command`] executes through the [`Engine`] seam. It is a state
//! machine over caller-supplied instants: the daemon sleeps until [`Ingress::next_deadline`].

mod coalesce;
mod command;
mod event;
mod forward;
mod localtime;
mod media;
mod pipeline;
mod prepare;
mod reaction;
mod text;
mod update;

pub use coalesce::Timing;
pub use command::{
    Command, Engine, EngineError, EngineStatus, Interrupted, NewSession, RouteSwitch, run_command,
};
pub use event::{MessageKind, ReplyContext};
pub use localtime::{LocalTime, ZONEINFO_DIRS, Zone, ZoneError};
pub use media::{Attachment, AttachmentKind, FileSource, RemoteFile};
pub use pipeline::{
    DEFAULT_MAX_MEDIA_BYTES, Ingress, IngressConfig, LOCAL_API_MAX_FILE_BYTES, Output,
    PUBLIC_API_MAX_FILE_BYTES, Ports, Stats,
};
pub use prepare::{Transcriber, Turn};
pub(crate) use reaction::REACTED_TEXT_CHARS;
pub use reaction::{Author, MessageTexts, ReactionConfig};
pub use update::{
    Chat, FileRef, Location, Message, MessageEntity, MessageOrigin, MessageReactionUpdated,
    ReactionType, Sticker, TextQuote, Update, UpdateKind, User, Venue,
};
