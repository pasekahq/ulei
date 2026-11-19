// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The lifecycle of `ulei-tg` (ADR-0032): `getMe`, the poller on its own thread, the ingress
//! loop on the caller's thread, and the stop that flushes the ingress before the offset is
//! confirmed.

use std::fmt;
use std::io;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::thread;
use std::time::Instant;

use super::settings::Settings;
use crate::bot_api::{self, Client, Poller, Stop, SystemClock};
use crate::ingress::{
    DEFAULT_MAX_MEDIA_BYTES, Ingress, IngressConfig, Output, Ports, ReactionConfig, Timing, Update,
};

/// Where the channel hands every [`Output`], in order, on the loop thread.
///
/// Seam: the renderer with `Sender` and the C6 client implement it.
pub trait Dispatch {
    /// Takes one output.
    fn dispatch(&mut self, output: Output);
}

/// Counts outputs by kind and keeps nothing else.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Turns for the engine.
    pub turns: u64,
    /// Owner commands.
    pub commands: u64,
    /// Notices to the owner.
    pub notices: u64,
}

impl Dispatch for Counts {
    fn dispatch(&mut self, output: Output) {
        match output {
            Output::Turn(_) => self.turns += 1,
            Output::Command { .. } => self.commands += 1,
            Output::Notice { .. } => self.notices += 1,
        }
    }
}

/// Why [`Channel::run`] ended without a clean stop.
#[derive(Debug)]
pub enum RunError {
    /// The poller thread could not be started.
    Spawn(io::Error),
    /// The poller thread ended abnormally.
    Poller,
    /// `deleteWebhook` at start or polling failed for good.
    Polling(bot_api::Error),
    /// The offset could not be confirmed at the stop.
    Confirm(bot_api::Error),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "poller thread: {e}"),
            Self::Poller => f.write_str("poller thread ended abnormally"),
            Self::Polling(e) => write!(f, "polling: {e}"),
            Self::Confirm(e) => write!(f, "confirming the offset: {e}"),
        }
    }
}

impl std::error::Error for RunError {}

/// The owner's Telegram channel, connected and ready to poll.
pub struct Channel {
    client: Client,
    ingress: Ingress,
    stop: Stop,
}

impl Channel {
    /// Checks the token with `getMe` and builds the ingress; `stop` ends [`Channel::run`].
    ///
    /// # Errors
    /// The classified error of `getMe`.
    pub fn start(settings: Settings, stop: Stop) -> Result<Self, bot_api::Error> {
        let client = Client::new(settings.bot);
        let me = client.get_me()?;
        let config = IngressConfig {
            owner_ids: settings.owner_ids,
            bot_username: me.username,
            inbox_dir: settings.attachments_dir,
            zone: settings.zone,
            max_file_bytes: client.config().max_file_bytes,
            max_media_bytes: DEFAULT_MAX_MEDIA_BYTES,
            timing: Timing {
                turn_coalesce: settings.coalesce_quiet,
                turn_coalesce_max: settings.coalesce_max,
                ..Timing::default()
            },
            reactions: ReactionConfig::default(),
        };
        let ports = Ports {
            files: Box::new(client.clone()),
            stt: None,
            texts: None,
        };
        Ok(Self {
            ingress: Ingress::new(config, ports),
            client,
            stop,
        })
    }

    /// Polls with the queued updates kept, feeds the ingress and hands its outputs to
    /// `dispatch` until the stop or a fatal polling error. Either way the pending ingress work is
    /// dispatched first; after a stop the offset is then confirmed.
    ///
    /// # Errors
    /// See [`RunError`].
    pub fn run(mut self, dispatch: &mut dyn Dispatch) -> Result<(), RunError> {
        let (batches, inbox) = sync_channel(1);
        let mut poller = Poller::new(self.client.clone(), Arc::new(SystemClock), self.stop);
        let polling = thread::Builder::new()
            .name("poller".into())
            .spawn(move || {
                let result = poll(&mut poller, &batches);
                (poller, result)
            })
            .map_err(RunError::Spawn)?;
        feed(&mut self.ingress, &inbox, dispatch);
        while let Some(due) = self.ingress.next_deadline() {
            hand(self.ingress.poll(due), dispatch);
        }
        let (poller, result) = polling.join().map_err(|_| RunError::Poller)?;
        result.map_err(RunError::Polling)?;
        poller.shutdown().map_err(RunError::Confirm)
    }
}

fn poll(
    poller: &mut Poller,
    batches: &SyncSender<Vec<bot_api::Update>>,
) -> Result<(), bot_api::Error> {
    poller.start(false)?;
    while let Some(batch) = poller.next_batch()? {
        // The loop thread holds the receiver until this thread ends.
        if batches.send(batch).is_err() {
            break;
        }
    }
    Ok(())
}

/// Accepts batches and runs deadlines until the poller thread is gone.
fn feed(
    ingress: &mut Ingress,
    inbox: &Receiver<Vec<bot_api::Update>>,
    dispatch: &mut dyn Dispatch,
) {
    loop {
        let received = match ingress.next_deadline() {
            Some(due) => inbox.recv_timeout(due.saturating_duration_since(Instant::now())),
            None => inbox.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match received {
            Ok(batch) => {
                for update in batch {
                    let id = update.update_id;
                    match Update::from_value(update.payload) {
                        Ok(update) => hand(ingress.accept(&update, Instant::now()), dispatch),
                        // The parser's message may quote the owner's text.
                        Err(_) => eprintln!("ulei-tg: update {id} skipped: unreadable"),
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        hand(ingress.poll(Instant::now()), dispatch);
    }
}

fn hand(outputs: Vec<Output>, dispatch: &mut dyn Dispatch) {
    for output in outputs {
        dispatch.dispatch(output);
    }
}
