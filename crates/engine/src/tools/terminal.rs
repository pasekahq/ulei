// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! `terminal`: `sh -c <command>` in its own process group, bounded in time and in captured
//! output.
//!
//! The whole group is killed when the call ends, so no process started by a call outlives it.

use std::io::{self, PipeReader, Read as _};
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use rustix::io::Errno;
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group, waitid};
use serde_json::{Map, Value, json};
use ulei_contracts::generated::c4g;

use crate::gateway::CancelHandle;

use super::{Args, Context, Output, TERMINAL, Tool, object_schema, resolve, type_name};

/// Timeout of a call that names none, in seconds.
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// Largest timeout a call may ask for, in seconds.
pub const MAX_TIMEOUT_SECS: u64 = 600;
/// Most bytes of combined stdout and stderr kept; the rest is counted and dropped.
pub const MAX_OUTPUT: usize = 32 * 1024;

/// How long output may still arrive after the group is killed; a process that left the group
/// can hold the pipe open forever.
const DRAIN: Duration = Duration::from_millis(200);
const CHUNK: usize = 8 * 1024;

/// A `Send + Clone` handle that stops the tool call or model stream running under it from
/// another thread.
///
/// Interrupting is permanent: the running shell's process group is killed at once, the running
/// gateway turn is cancelled, and every later call through the same handle fails without running.
#[derive(Debug, Clone, Default)]
pub struct Interrupt {
    state: Arc<Mutex<State>>,
}

#[derive(Debug, Default)]
struct State {
    interrupted: bool,
    running: Option<(Pid, Sender<Event>)>,
    stream: Option<CancelHandle>,
}

impl Interrupt {
    /// A handle that is not interrupted.
    pub fn new() -> Self {
        Self::default()
    }

    /// Interrupts the running call or gateway turn, if any, and every later one.
    pub fn interrupt(&self) {
        let stream = {
            let mut state = self.lock();
            state.interrupted = true;
            if let Some((group, wake)) = state.running.take() {
                // ESRCH only: the group already exited.
                let _ = kill_process_group(group, Signal::KILL);
                let _ = wake.send(Event::Interrupted);
            }
            state.stream.take()
        };
        if let Some(stream) = stream {
            // The local stream ends even when the cancel call fails, and the reader sees that.
            let _ = stream.cancel();
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

    /// Registers the running gateway turn; false when the handle is already interrupted.
    pub(crate) fn attach_stream(&self, stream: CancelHandle) -> bool {
        let mut state = self.lock();
        if state.interrupted {
            return false;
        }
        state.stream = Some(stream);
        true
    }

    pub(crate) fn detach_stream(&self) {
        self.lock().stream = None;
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

const DESCRIPTION: &str = "Execute shell commands on a Linux environment.

Do NOT use cat/head/tail (use read_file), grep/rg/find/ls (use search_files), sed/awk (use patch), \
or echo/heredoc file creation (use write_file). Reserve terminal for: builds, installs, git, \
processes, scripts, network, package managers, and anything that needs a shell.

Foreground (default): returns INSTANTLY when the command finishes, even with a high timeout \
\u{2014} set timeout generously for long builds.
Working directory: use 'workdir' for per-command cwd.
";

/// Characters besides letters and digits a `workdir` may hold.
const WORKDIR_SAFE: &str = "/\\:_-.~ +@=,";

pub(super) struct Terminal;

impl Tool for Terminal {
    fn definition(&self) -> c4g::Tool {
        c4g::Tool {
            name: TERMINAL.into(),
            description: DESCRIPTION.into(),
            input_schema: object_schema(
                json!({
                    "command": {
                        "type": "string",
                        "description": "The command to execute on the VM"
                    },
                    "timeout": {
                        "type": "integer",
                        "description": format!(
                            "Max seconds to wait (default: {DEFAULT_TIMEOUT_SECS}, foreground max: \
                             {MAX_TIMEOUT_SECS}). Returns INSTANTLY when command finishes \u{2014} set \
                             high for long tasks, you won't wait unnecessarily. Foreground timeout \
                             above {MAX_TIMEOUT_SECS}s is rejected."
                        ),
                        "minimum": 1
                    },
                    "workdir": {
                        "type": "string",
                        "description": "Working directory for this command (absolute path). \
                                        Defaults to the session working directory."
                    }
                }),
                &["command"],
            ),
        }
    }

    fn run(&self, input: &Map<String, Value>, ctx: Context<'_>) -> Output {
        match parse(input, ctx) {
            Ok((command, timeout, cwd)) => run(command, timeout, &cwd, ctx)
                .unwrap_or_else(|e| Output::error(format!("Failed to execute command: {e}"))),
            Err(e) => Output::error(e),
        }
    }
}

/// The command, the timeout in seconds and the working directory of a call.
fn parse<'a>(
    input: &'a Map<String, Value>,
    ctx: Context<'_>,
) -> Result<(&'a str, u64, PathBuf), String> {
    let args = Args(input);
    let command = input.get("command");
    let Some(command) = command.and_then(Value::as_str) else {
        return Err(format!(
            "Invalid command: expected string, got {}",
            type_name(command)
        ));
    };
    let timeout = match args.opt_i64("timeout")? {
        None => DEFAULT_TIMEOUT_SECS,
        Some(t) if t <= 0 => {
            return Err(format!(
                "timeout must be a positive number of seconds (got {t})."
            ));
        }
        Some(t) => match u64::try_from(t) {
            Ok(t) if t <= MAX_TIMEOUT_SECS => t,
            _ => {
                return Err(format!(
                    "Foreground timeout {t}s exceeds the maximum of {MAX_TIMEOUT_SECS}s."
                ));
            }
        },
    };
    let cwd = match args.opt_str("workdir")? {
        Some(workdir) if !workdir.is_empty() => {
            if let Some(ch) = workdir
                .chars()
                .find(|&c| c.is_control() || !(c.is_alphanumeric() || WORKDIR_SAFE.contains(c)))
            {
                return Err(format!(
                    "Blocked: workdir contains disallowed character {}. Use a simple filesystem \
                     path without shell metacharacters.",
                    char_repr(ch)
                ));
            }
            resolve(ctx.cwd, workdir)
        }
        _ => ctx.cwd.to_path_buf(),
    };
    Ok((command, timeout, cwd))
}

/// `ch` quoted as the model's training shows a one-character string.
fn char_repr(ch: char) -> String {
    match ch {
        '\'' => "\"'\"".into(),
        '\n' => "'\\n'".into(),
        '\t' => "'\\t'".into(),
        '\r' => "'\\r'".into(),
        c if c.is_control() => format!("'\\x{:02x}'", u32::from(c)),
        c => format!("'{c}'"),
    }
}

fn run(command: &str, timeout: u64, cwd: &Path, ctx: Context<'_>) -> io::Result<Output> {
    let (reader, writer) = io::pipe()?;
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(writer.try_clone()?)
        .stderr(writer)
        .process_group(0)
        .spawn()?;
    let group = Pid::from_child(&child);
    let capture = Arc::new(Mutex::new(Capture::default()));
    let (events, inbox) = mpsc::channel();

    let deadline = Instant::now() + Duration::from_secs(timeout);
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
    Ok(render(capture, end, status, timeout))
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
        .name("terminal-output".into())
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
        .name("terminal-wait".into())
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

fn render(capture: Capture, end: End, status: ExitStatus, timeout: u64) -> Output {
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
                &format!("[timed out after {timeout} s; process group killed]"),
            );
            failed(text)
        }
        End::Interrupted => {
            push_line(&mut text, "[interrupted; process group killed]");
            failed(text)
        }
    }
}

/// A call that ended without its command finishing; the text is the captured output.
fn failed(text: String) -> Output {
    Output {
        content: text,
        is_error: true,
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
