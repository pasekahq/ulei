// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The lifecycle of `ulei-tg` (ADR-0032): `getMe`, the poller on its own thread, the ingress
//! loop on the caller's thread, and the stop that flushes the ingress before the offset is
//! confirmed. With a guest gate (ADR-0083) the owner's protocol updates go to its worker thread
//! before the ingress, and the ones it does not take come back to the loop.

use std::fmt;
use std::io;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::thread;
use std::time::Instant;

use super::guest_gate::{GateWorker, GuestGate};
use super::settings::Settings;
use crate::bot_api::{self, Client, Poller, Stop, SystemClock};
use crate::ingress::{
    DEFAULT_MAX_MEDIA_BYTES, Ingress, IngressConfig, MessageTexts, Output, Ports, Timing,
    Transcriber, Update, guest,
};
use crate::stt::Stt;

/// Where the channel hands every [`Output`], in order, on the loop thread.
///
/// Seam: [`super::Link`] implements it.
pub trait Dispatch {
    /// Takes one output.
    fn dispatch(&mut self, output: Output);
}

/// Why [`Channel::run`] ended without a clean stop.
#[derive(Debug)]
pub enum RunError {
    /// The poller or the guest gate's thread could not be started.
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
    owner_ids: Vec<i64>,
    gate: Option<GuestGate>,
}

/// What the loop thread receives.
enum Input {
    /// A batch of the poller.
    Batch(Vec<bot_api::Update>),
    /// An update the guest gate did not take, to go the usual way.
    Back(Box<Update>),
    /// The poller thread is ending.
    Closed,
}

/// Tells the loop the poller thread ended, also when it ends by a panic.
struct Closing(SyncSender<Input>);

impl Drop for Closing {
    fn drop(&mut self) {
        let _ = self.0.send(Input::Closed);
    }
}

impl Channel {
    /// Checks the token with `getMe` and builds the ingress, which looks up reacted-to messages
    /// in `texts`; `stop` ends [`Channel::run`].
    ///
    /// # Errors
    /// The classified error of `getMe`.
    pub fn start(
        settings: Settings,
        stop: Stop,
        texts: Box<dyn MessageTexts>,
    ) -> Result<Self, bot_api::Error> {
        let client = Client::new(settings.bot);
        let me = client.get_me()?;
        let owner_ids = settings.owner_ids.clone();
        let gate = settings.guest_gate.map(GuestGate::new);
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
            reactions: settings.reactions,
        };
        let ports = Ports {
            files: Box::new(client.clone()),
            stt: settings
                .stt
                .map(|config| Box::new(Stt::new(config)) as Box<dyn Transcriber>),
            texts: Some(texts),
        };
        Ok(Self {
            ingress: Ingress::new(config, ports),
            client,
            stop,
            owner_ids,
            gate,
        })
    }

    /// The Bot API client, to share with the senders.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Polls with the queued updates kept, feeds the ingress and hands its outputs to
    /// `dispatch` until the stop or a fatal polling error. Either way the pending ingress work is
    /// dispatched first; after a stop the offset is then confirmed.
    ///
    /// # Errors
    /// See [`RunError`].
    pub fn run(mut self, dispatch: &mut dyn Dispatch) -> Result<(), RunError> {
        let (batches, inbox) = sync_channel(1);
        let worker = match self.gate.take() {
            Some(gate) => {
                let back = batches.clone();
                Some(
                    GateWorker::start(gate, move |update| back.send(Input::Back(update)).is_ok())
                        .map_err(RunError::Spawn)?,
                )
            }
            None => None,
        };
        let mut poller = Poller::new(self.client.clone(), Arc::new(SystemClock), self.stop);
        let polling = thread::Builder::new()
            .name("poller".into())
            .spawn(move || {
                let closing = Closing(batches);
                let result = poll(&mut poller, &closing.0);
                drop(closing);
                (poller, result)
            })
            .map_err(RunError::Spawn)?;
        let gate = worker.as_ref().map(|w| (w, self.owner_ids.as_slice()));
        feed(&mut self.ingress, &inbox, dispatch, gate);
        // A gate call still running is not waited for: its update is dropped with the process.
        drop(worker);
        while let Some(due) = self.ingress.next_deadline() {
            hand(self.ingress.poll(due), dispatch);
        }
        let (poller, result) = polling.join().map_err(|_| RunError::Poller)?;
        result.map_err(RunError::Polling)?;
        poller.shutdown().map_err(RunError::Confirm)
    }
}

fn poll(poller: &mut Poller, batches: &SyncSender<Input>) -> Result<(), bot_api::Error> {
    poller.start(false)?;
    while let Some(batch) = poller.next_batch()? {
        // The loop thread holds the receiver until this thread ends.
        if batches.send(Input::Batch(batch)).is_err() {
            break;
        }
    }
    Ok(())
}

/// Accepts batches and the updates the gate gave back, and runs deadlines, until the poller
/// thread is gone. With `gate`, the owner's protocol updates go to the gate's worker first.
fn feed(
    ingress: &mut Ingress,
    inbox: &Receiver<Input>,
    dispatch: &mut dyn Dispatch,
    gate: Option<(&GateWorker, &[i64])>,
) {
    loop {
        let received = match ingress.next_deadline() {
            Some(due) => inbox.recv_timeout(due.saturating_duration_since(Instant::now())),
            None => inbox.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match received {
            Ok(Input::Batch(batch)) => {
                for update in batch {
                    let id = update.update_id;
                    let forward =
                        gate.and_then(|(_, owners)| guest::classify(&update.payload, owners));
                    let Ok(parsed) = Update::from_value(update.payload) else {
                        // The parser's message may quote the owner's text.
                        eprintln!("ulei-tg: update {id} skipped: unreadable");
                        continue;
                    };
                    let usual = match (forward, gate) {
                        (Some(forward), Some((worker, _))) => {
                            worker.send(forward, Box::new(parsed)).err()
                        }
                        _ => Some(Box::new(parsed)),
                    };
                    if let Some(update) = usual {
                        hand(ingress.accept(&update, Instant::now()), dispatch);
                    }
                }
            }
            Ok(Input::Back(update)) => hand(ingress.accept(&update, Instant::now()), dispatch),
            Err(RecvTimeoutError::Timeout) => {}
            Ok(Input::Closed) | Err(RecvTimeoutError::Disconnected) => {
                // What the gate already gave back still goes; a call in flight is not waited for.
                while let Ok(input) = inbox.try_recv() {
                    if let Input::Back(update) = input {
                        hand(ingress.accept(&update, Instant::now()), dispatch);
                    }
                }
                return;
            }
        }
        hand(ingress.poll(Instant::now()), dispatch);
    }
}

fn hand(outputs: Vec<Output>, dispatch: &mut dyn Dispatch) {
    for output in outputs {
        dispatch.dispatch(output);
    }
}
