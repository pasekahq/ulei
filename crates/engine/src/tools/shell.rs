//! `shell`: `sh -c <command>` in its own process group, bounded in time and in captured output.
//!
//! The whole group is killed when the call ends, so no process started by a call outlives it.

use std::io::{self, PipeReader, Read as _};
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use rustix::io::Errno;
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group, waitid};
use serde_json::{Map, Value, json};
use ulei_contracts::generated::c4g;

use super::{Args, Context, Output, SHELL, Tool, object_schema};

/// Timeout of a call that names none, in milliseconds.
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// Largest timeout a call may ask for, in milliseconds.
pub const MAX_TIMEOUT_MS: u64 = 600_000;
/// Most bytes of combined stdout and stderr kept; the rest is counted and dropped.
pub const MAX_OUTPUT: usize = 32 * 1024;

/// How long output may still arrive after the group is killed; a process that left the group
/// can hold the pipe open forever.
const DRAIN: Duration = Duration::from_millis(200);
const CHUNK: usize = 8 * 1024;

/// A `Send + Clone` handle that stops the tool call running under it from another thread.
///
/// Interrupting is permanent: the running shell's process group is killed at once and every
/// later call through the same handle fails without running.
#[derive(Debug, Clone, Default)]
pub struct Interrupt {
    state: Arc<Mutex<State>>,
}

#[derive(Debug, Default)]
struct State {
    interrupted: bool,
    running: Option<(Pid, Sender<Event>)>,
}

impl Interrupt {
    /// A handle that is not interrupted.
    pub fn new() -> Self {
        Self::default()
    }

    /// Interrupts the running call, if any, and every later one.
    pub fn interrupt(&self) {
        let mut state = self.lock();
        state.interrupted = true;
        if let Some((group, wake)) = state.running.take() {
            // ESRCH only: the group already exited.
            let _ = kill_process_group(group, Signal::KILL);
            let _ = wake.send(Event::Interrupted);
        }
    }

    /// Whether [`Interrupt::interrupt`] was called.
    pub fn is_interrupted(&self) -> bool {
        self.lock().interrupted
    }

    /// Registers the running group; false when the handle is already interrupted.
    fn attach(&self, group: Pid, wake: Sender<Event>) -> bool {
        let mut state = self.lock();
        if state.interrupted {
            return false;
        }
        state.running = Some((group, wake));
        true
    }

    /// Must run before the group leader is reaped, so a reused pid is never signalled.
    fn detach(&self) {
        self.lock().running = None;
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Debug)]
enum Event {
    Exited,
    Eof,
    Interrupted,
}

#[derive(Debug, Clone, Copy)]
enum End {
    Exited,
    TimedOut,
    Interrupted,
}

#[derive(Debug, Default)]
struct Capture {
    bytes: Vec<u8>,
    dropped: u64,
    error: Option<io::Error>,
}

impl Capture {
    fn push(&mut self, chunk: &[u8]) {
        let keep = chunk.len().min(MAX_OUTPUT - self.bytes.len());
        self.bytes.extend_from_slice(&chunk[..keep]);
        self.dropped += (chunk.len() - keep) as u64;
    }
}

pub(super) struct Shell;

impl Tool for Shell {
    fn definition(&self) -> c4g::Tool {
        c4g::Tool {
            name: SHELL.into(),
            description: format!(
                "Run a command with `sh -c` in the working directory. Returns combined stdout and \
                 stderr (at most {MAX_OUTPUT} bytes) and the exit status. Processes it starts are \
                 killed when it ends."
            ),
            input_schema: object_schema(
                json!({
                    "command": {"type": "string", "description": "Shell command."},
                    "timeout_ms": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_TIMEOUT_MS,
                        "description": format!("Timeout in milliseconds, default {DEFAULT_TIMEOUT_MS}.")
                    }
                }),
                &["command"],
            ),
        }
    }

    fn run(&self, input: &Map<String, Value>, ctx: Context<'_>) -> Output {
        let parsed = Args::new(input, &["command", "timeout_ms"]).and_then(|args| {
            let command = args.str("command")?;
            let timeout_ms = args.opt_u64("timeout_ms")?.unwrap_or(DEFAULT_TIMEOUT_MS);
            if timeout_ms == 0 || timeout_ms > MAX_TIMEOUT_MS {
                return Err(format!(
                    "field `timeout_ms` must be between 1 and {MAX_TIMEOUT_MS}"
                ));
            }
            Ok((command, timeout_ms))
        });
        match parsed {
            Ok((command, timeout_ms)) => run(command, timeout_ms, ctx)
                .unwrap_or_else(|e| Output::error(format!("cannot run the command: {e}"))),
            Err(e) => Output::error(e),
        }
    }
}

fn run(command: &str, timeout_ms: u64, ctx: Context<'_>) -> io::Result<Output> {
    let (reader, writer) = io::pipe()?;
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(ctx.cwd)
        .stdin(Stdio::null())
        .stdout(writer.try_clone()?)
        .stderr(writer)
        .process_group(0)
        .spawn()?;
    let group = Pid::from_child(&child);
    let capture = Arc::new(Mutex::new(Capture::default()));
    let (events, inbox) = mpsc::channel();

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let supervised = if ctx.interrupt.attach(group, events.clone()) {
        start_threads(reader, group, &capture, &events)
            .map(|waiter| (wait_for_end(&inbox, deadline, ctx.interrupt), Some(waiter)))
    } else {
        Ok((End::Interrupted, None))
    };
    drop(events);

    ctx.interrupt.detach();
    // ESRCH only: every member already exited.
    let _ = kill_process_group(group, Signal::KILL);
    let (end, waiter) = supervised.inspect_err(|_| {
        let _ = child.wait();
    })?;
    if let Some(waiter) = waiter {
        let _ = waiter.join();
    }
    let status = child.wait()?;
    drain(&inbox);

    let capture = std::mem::take(&mut *lock(&capture));
    Ok(render(capture, end, status, timeout_ms))
}

/// Starts the output reader and the exit waiter; returns the waiter to join after the kill.
fn start_threads(
    mut reader: PipeReader,
    group: Pid,
    capture: &Arc<Mutex<Capture>>,
    events: &Sender<Event>,
) -> io::Result<thread::JoinHandle<()>> {
    let sink = Arc::clone(capture);
    let eof = events.clone();
    thread::Builder::new()
        .name("shell-output".into())
        .spawn(move || {
            let mut chunk = [0u8; CHUNK];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => lock(&sink).push(&chunk[..n]),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        lock(&sink).error = Some(e);
                        break;
                    }
                }
            }
            let _ = eof.send(Event::Eof);
        })?;
    let exited = events.clone();
    thread::Builder::new()
        .name("shell-wait".into())
        .spawn(move || {
            // NOWAIT leaves the leader a zombie, so its pid, the group id, stays reserved until
            // the supervisor reaps it after the kill.
            let options = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT;
            while let Err(Errno::INTR) = waitid(WaitId::Pid(group), options) {}
            let _ = exited.send(Event::Exited);
        })
}

/// An interrupted handle names the end even when the exit of the killed leader arrives first.
fn wait_for_end(inbox: &mpsc::Receiver<Event>, deadline: Instant, interrupt: &Interrupt) -> End {
    let end = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match inbox.recv_timeout(left) {
            Ok(Event::Eof) => {}
            Ok(Event::Exited) | Err(RecvTimeoutError::Disconnected) => break End::Exited,
            Ok(Event::Interrupted) => break End::Interrupted,
            Err(RecvTimeoutError::Timeout) => break End::TimedOut,
        }
    };
    if interrupt.is_interrupted() {
        End::Interrupted
    } else {
        end
    }
}

/// Waits up to [`DRAIN`] for the reader to see the end of the output.
fn drain(inbox: &mpsc::Receiver<Event>) {
    let deadline = Instant::now() + DRAIN;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match inbox.recv_timeout(left) {
            Ok(Event::Eof) | Err(_) => return,
            Ok(Event::Exited | Event::Interrupted) => {}
        }
    }
}

fn render(capture: Capture, end: End, status: ExitStatus, timeout_ms: u64) -> Output {
    let mut text = String::from_utf8_lossy(&capture.bytes).into_owned();
    if capture.dropped > 0 {
        push_line(
            &mut text,
            &format!("[output truncated: {} more bytes dropped]", capture.dropped),
        );
    }
    if let Some(e) = capture.error {
        push_line(&mut text, &format!("[reading the output failed: {e}]"));
    }
    match end {
        End::Exited => {
            let status = match (status.code(), status.signal()) {
                (Some(code), _) => format!("[exit code {code}]"),
                (None, Some(signal)) => format!("[killed by signal {signal}]"),
                (None, None) => format!("[exit status {status}]"),
            };
            push_line(&mut text, &status);
            Output::ok(text)
        }
        End::TimedOut => {
            push_line(
                &mut text,
                &format!("[timed out after {timeout_ms} ms; process group killed]"),
            );
            Output::error(text)
        }
        End::Interrupted => {
            push_line(&mut text, "[interrupted; process group killed]");
            Output::error(text)
        }
    }
}

fn push_line(text: &mut String, line: &str) {
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(line);
}

fn lock(capture: &Mutex<Capture>) -> MutexGuard<'_, Capture> {
    capture.lock().unwrap_or_else(PoisonError::into_inner)
}
