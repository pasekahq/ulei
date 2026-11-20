// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! The [`Dispatch`] of `ulei-tg` (ADR-0032 section 2, ADR-0033, ADR-0034, ADR-0035): notices go
//! straight to the owner; turns and commands go to the engine on a thread of their own, which
//! echoes a turn's voice transcripts to the owner before submitting it, whose replies and
//! failures reach the owner through the [`Outbox`], and every accepted turn is read to its end
//! by its chat's reader.

use std::collections::HashMap;
use std::io::{self, Write};
use std::panic::{self, AssertUnwindSafe};
use std::process;
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};

use super::channel::Dispatch;
use super::follow::Followers;
use super::outbound::{Closed, Outbox};
use crate::c6::{self, ChatSession};
use crate::ingress::{Command, Engine, Output, Turn, Zone, run_command};

/// What the owner sees when the engine cannot take a turn or run a command: the reference's
/// answer to a failed agent run, without the raw error.
pub const ENGINE_FAILED: &str =
    "Sorry, I encountered an unexpected error.\nTry again or use /reset to start a fresh session.";

/// Outputs by kind that reached [`Link`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Turns for the engine.
    pub turns: u64,
    /// Owner commands.
    pub commands: u64,
    /// Notices to the owner.
    pub notices: u64,
}

enum Job {
    Turn(Turn),
    Command {
        chat_id: i64,
        message_id: i64,
        command: Command,
    },
}

/// Hands turns and commands to the `engine` thread, which makes the C6 calls in order, so the
/// loop thread never waits for the engine.
pub struct Link {
    jobs: mpsc::Sender<Job>,
    worker: JoinHandle<()>,
    outbox: Outbox,
    counts: Counts,
}

impl Link {
    /// Starts the engine thread over `client`; replies and notices go out through `outbox`,
    /// with the dates of `/status` in `zone`, and voice transcripts are echoed when
    /// `echo_transcripts` holds.
    ///
    /// # Errors
    /// The thread could not be spawned.
    pub fn start(
        client: c6::Client,
        outbox: Outbox,
        zone: Zone,
        echo_transcripts: bool,
    ) -> io::Result<Self> {
        let (jobs, inbox) = mpsc::channel();
        let worker = Worker {
            followers: Followers::new(client.clone(), outbox.clone()),
            sessions: Sessions {
                client,
                chats: HashMap::new(),
            },
            outbox: outbox.clone(),
            zone,
            echo_transcripts,
        };
        let worker = spawn_vital("engine", move || worker.run(&inbox))?;
        Ok(Self {
            jobs,
            worker,
            outbox,
            counts: Counts::default(),
        })
    }

    /// Outputs dispatched so far.
    pub fn counts(&self) -> Counts {
        self.counts
    }

    /// Waits until every queued turn and command has been handed to the engine, then stops
    /// reading the turns at once.
    ///
    /// # Errors
    /// [`Closed`] when the engine thread ended abnormally.
    pub fn finish(self) -> Result<(), Closed> {
        drop(self.jobs);
        self.worker.join().map_err(|_| Closed)
    }

    fn queue(&self, chat_id: i64, job: Job) {
        if self.jobs.send(job).is_err() {
            eprintln!("ulei-tg: engine thread gone, work for chat {chat_id} dropped");
        }
    }
}

impl Dispatch for Link {
    fn dispatch(&mut self, output: Output) {
        match output {
            Output::Turn(turn) => {
                self.counts.turns += 1;
                self.queue(turn.chat_id, Job::Turn(turn));
            }
            Output::Command {
                chat_id,
                message_id,
                command,
            } => {
                self.counts.commands += 1;
                self.queue(
                    chat_id,
                    Job::Command {
                        chat_id,
                        message_id,
                        command,
                    },
                );
            }
            Output::Notice {
                chat_id,
                reply_to,
                text,
            } => {
                self.counts.notices += 1;
                deliver(&self.outbox, chat_id, Some(reply_to), text);
            }
        }
    }
}

struct Worker {
    followers: Followers,
    sessions: Sessions,
    outbox: Outbox,
    zone: Zone,
    echo_transcripts: bool,
}

/// The session of each chat that sent work, on one client.
struct Sessions {
    client: c6::Client,
    chats: HashMap<i64, ChatSession>,
}

impl Sessions {
    fn of(&mut self, chat_id: i64) -> &mut ChatSession {
        self.chats
            .entry(chat_id)
            .or_insert_with(|| ChatSession::new(self.client.clone(), chat_id))
    }
}

impl Worker {
    fn run(mut self, inbox: &Receiver<Job>) {
        for job in inbox {
            match job {
                Job::Turn(turn) => self.turn(&turn),
                Job::Command {
                    chat_id,
                    message_id,
                    command,
                } => self.command(chat_id, message_id, &command),
            }
        }
        self.followers.stop();
    }

    fn turn(&mut self, turn: &Turn) {
        if self.echo_transcripts {
            for transcript in &turn.transcripts {
                if let Err(e) = self
                    .outbox
                    .echo(turn.chat_id, format!("🎙️ \"{transcript}\""))
                {
                    eprintln!("ulei-tg: echo to chat {} dropped: {e}", turn.chat_id);
                }
            }
        }
        let session = self.sessions.of(turn.chat_id);
        let accepted = session.submit_turn(turn);
        let failure = session.take_failure();
        let followed = match accepted {
            Ok(id) => self.followers.follow(turn.chat_id, turn.message_id, id),
            Err(e) => {
                failed(turn.chat_id, &e.message, failure.as_ref());
                deliver(&self.outbox, turn.chat_id, turn.message_id, ENGINE_FAILED);
                return;
            }
        };
        if let Err(e) = followed {
            eprintln!("ulei-tg: reader of chat {} not started: {e}", turn.chat_id);
            deliver(&self.outbox, turn.chat_id, turn.message_id, ENGINE_FAILED);
        }
    }

    /// Runs `command`; an engine C6 error is the reply only for `/model <alias>`, as the
    /// reference shows a failed switch.
    fn command(&mut self, chat_id: i64, message_id: i64, command: &Command) {
        let session = self.sessions.of(chat_id);
        let reply = run_command(session, command, &self.zone);
        let failure = session.take_failure();
        let text = match (reply, &failure) {
            (Ok(text), None | Some(c6::Error::Remote { .. })) => text,
            (Ok(_), Some(e)) => {
                failed(chat_id, &e.to_string(), failure.as_ref());
                ENGINE_FAILED.to_owned()
            }
            (Err(e), _) => {
                failed(chat_id, &e.message, failure.as_ref());
                ENGINE_FAILED.to_owned()
            }
        };
        deliver(&self.outbox, chat_id, Some(message_id), text);
    }
}

/// Spawns thread `name` running `body`. A panic in `body` writes one stderr line and exits the
/// process with status 1 at once, as the abort of the release build does, so no later work is
/// dropped and the updates not yet confirmed are polled again after the restart.
///
/// # Errors
/// The thread could not be spawned.
pub fn spawn_vital(name: &str, body: impl FnOnce() + Send + 'static) -> io::Result<JoinHandle<()>> {
    let label = name.to_owned();
    thread::Builder::new().name(label.clone()).spawn(move || {
        let Err(payload) = panic::catch_unwind(AssertUnwindSafe(body)) else {
            return;
        };
        let message = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("no message");
        let _ = writeln!(
            io::stderr().lock(),
            "ulei-tg: {label} thread panicked: {}",
            message.replace('\n', " ")
        );
        process::exit(1);
    })
}

fn failed(chat_id: i64, message: &str, failure: Option<&c6::Error>) {
    match failure {
        Some(e) => eprintln!("ulei-tg: engine call for chat {chat_id} failed: {e}"),
        None => eprintln!("ulei-tg: engine call for chat {chat_id} failed: {message}"),
    }
}

fn deliver(outbox: &Outbox, chat_id: i64, reply_to: Option<i64>, text: impl Into<String>) {
    if let Err(e) = outbox.deliver(chat_id, reply_to, text, true) {
        eprintln!("ulei-tg: reply to chat {chat_id} dropped: {e}");
    }
}
