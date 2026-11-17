// Port: THIRD_PARTY_NOTICES.md#ref-agent
//! `terminal`: `bash -c <command>` in its own process group, bounded in time and in retained
//! output, answered with the JSON result `output`, `exit_code`, `error`, secrets redacted.
//!
//! The whole group is killed when the call ends, so no process started by a call outlives it.

mod ansi;
mod collector;
mod exit_note;

use std::fs;
use std::io::{self, PipeReader, Read as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use rustix::io::Errno;
use rustix::process::{
    Pid, Signal, WaitId, WaitIdOptions, kill_process_group, test_kill_process_group, waitid,
};
use serde_json::{Map, Value, json};
use ulei_contracts::generated::c4g;

use self::collector::{Collector, Utf8Decoder};
use super::repr::repr;
use crate::gateway::CancelHandle;

use super::redact::{Flags, Redactor};
use super::{Args, Context, Output, TERMINAL, Tool, json_object, json_str, object_schema};
use super::{is_py_space, resolve, thousands, type_name};

pub use self::collector::SPILL_CAP_CHARS;

/// Timeout of a call that names none, in seconds.
pub const DEFAULT_TIMEOUT_SECS: u64 = 180;
/// Largest timeout a call may ask for, in seconds.
pub const MAX_TIMEOUT_SECS: u64 = 600;
/// Most characters of output a result shows; longer output keeps its head and tail.
pub const MAX_OUTPUT_CHARS: usize = 50_000;
/// How long a group sent SIGTERM at a timeout or an interrupt has before SIGKILL.
pub const KILL_GRACE: Duration = Duration::from_secs(1);

/// How long a group sent SIGKILL is waited for.
const KILL_WAIT: Duration = Duration::from_secs(2);
const GROUP_POLL: Duration = Duration::from_millis(50);
/// How long output may still arrive after the group is gone; a process that left the group
/// can hold the pipe open forever.
const DRAIN: Duration = Duration::from_millis(200);
const CHUNK: usize = 8 * 1024;

/// A `Send + Clone` handle that stops the tool call or model stream running under it from
/// another thread.
///
/// Interrupting is permanent: the running command's process group gets SIGTERM and, after
/// [`KILL_GRACE`], SIGKILL; the running gateway turn is cancelled; and every later call through
/// the same handle fails without running.
#[derive(Debug, Clone, Default)]
pub struct Interrupt {
    state: Arc<Mutex<State>>,
}

#[derive(Debug, Default)]
struct State {
    interrupted: bool,
    running: Option<Sender<Event>>,
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
            if let Some(wake) = state.running.take() {
                // The supervisor ended already when the receiver is gone.
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

    /// Registers the supervisor of the running command; false when already interrupted.
    fn attach(&self, wake: Sender<Event>) -> bool {
        let mut state = self.lock();
        if state.interrupted {
            return false;
        }
        state.running = Some(wake);
        true
    }

    fn detach(&self) {
        self.lock().running = None;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    Exited,
    TimedOut,
    Interrupted,
}

const DESCRIPTION: &str = "Execute shell commands on a Linux environment.

Do NOT use cat/head/tail (use read_file), grep/rg/find/ls (use search_files), sed/awk (use patch), \
or echo/heredoc file creation (use write_file). Reserve terminal for: builds, installs, git, \
processes, scripts, network, package managers, and anything that needs a shell.
NEVER pipe a build/test command through tail/head/cat to shorten output (e.g. `cargo build | tail \
-20`): output is auto-truncated with the full text saved to a file, and the pipe makes exit_code \
report the LAST pipeline command's status (tail's 0), masking real failures. Run the command bare; \
the same applies to `cmd || echo failed`, which also masks the exit code.

Foreground (default): returns INSTANTLY when the command finishes, even with a high timeout \
\u{2014} set timeout generously for long builds.
Working directory: use 'workdir' for per-command cwd.
";

/// Characters besides letters and digits a `workdir` may hold.
const WORKDIR_SAFE: &str = "/\\:_-.~ +@=,";

/// The `terminal` tool; spill files of overflowing output go to `spill_dir`.
pub(super) struct Terminal {
    pub(super) spill_dir: PathBuf,
    pub(super) redactor: Redactor,
}

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
                             above {MAX_TIMEOUT_SECS}s is rejected; use background=true for longer \
                             commands."
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
        let (command, timeout, cwd) = match parse(input, ctx) {
            Ok(call) => call,
            Err(out) => return out,
        };
        match execute(command, timeout, &cwd, &self.spill_dir, ctx.interrupt) {
            Ok(finished) => finished.into_output(command, self.redactor),
            Err(e) => {
                let error = format!("Command execution failed: {e}");
                let forced = Flags {
                    force: true,
                    ..Flags::default()
                };
                rejected(None, &self.redactor.sensitive_text(&error, forced))
            }
        }
    }
}

/// The command, the timeout in seconds and the working directory of a call, or its rejection.
fn parse<'a>(
    input: &'a Map<String, Value>,
    ctx: Context<'_>,
) -> Result<(&'a str, u64, PathBuf), Output> {
    let args = Args(input);
    let command = input.get("command");
    let Some(command) = command.and_then(Value::as_str) else {
        return Err(rejected(
            Some("error"),
            &format!(
                "Invalid command: expected string, got {}",
                type_name(command)
            ),
        ));
    };
    let timeout = match args.opt_i64("timeout").map_err(Output::error)? {
        None => DEFAULT_TIMEOUT_SECS,
        Some(t) if t <= 0 => {
            return Err(Output::error(format!(
                "timeout must be a positive number of seconds (got {t})."
            )));
        }
        Some(t) => match u64::try_from(t) {
            Ok(t) if t <= MAX_TIMEOUT_SECS => t,
            _ => {
                return Err(Output::error(format!(
                    "Foreground timeout {t}s exceeds the maximum of {MAX_TIMEOUT_SECS}s."
                )));
            }
        },
    };
    let cwd = match args.opt_str("workdir").map_err(Output::error)? {
        Some(workdir) if !workdir.is_empty() => {
            if let Some(ch) = workdir
                .chars()
                .find(|&c| c.is_control() || !(c.is_alphanumeric() || WORKDIR_SAFE.contains(c)))
            {
                return Err(rejected(
                    Some("blocked"),
                    &format!(
                        "Blocked: workdir contains disallowed character {}. Use a simple \
                         filesystem path without shell metacharacters.",
                        repr(ch.encode_utf8(&mut [0; 4]))
                    ),
                ));
            }
            resolve(ctx.cwd, workdir)
        }
        _ => ctx.cwd.to_path_buf(),
    };
    Ok((command, timeout, cwd))
}

/// A call that ran no command: empty output, exit code -1, the error and an optional status.
fn rejected(status: Option<&str>, error: &str) -> Output {
    let mut fields = vec![
        ("output", json_str("")),
        ("exit_code", "-1".to_owned()),
        ("error", json_str(error)),
    ];
    if let Some(status) = status {
        fields.push(("status", json_str(status)));
    }
    Output {
        content: json_object(fields),
        is_error: true,
    }
}

/// `e` as Python's `str()` of the `OSError` subclass its errno maps to,
/// `Class: [Errno N] text`, followed by `: '<filename>'` when the error names one.
fn python_os_error(e: &io::Error, filename: Option<&Path>) -> String {
    let Some(errno) = e.raw_os_error() else {
        return format!("OSError: {e}");
    };
    let class = match errno {
        1 | 13 => "PermissionError",
        2 => "FileNotFoundError",
        3 => "ProcessLookupError",
        4 => "InterruptedError",
        10 => "ChildProcessError",
        11 => "BlockingIOError",
        17 => "FileExistsError",
        20 => "NotADirectoryError",
        21 => "IsADirectoryError",
        _ => "OSError",
    };
    let text = e.to_string();
    let text = text
        .strip_suffix(&format!(" (os error {errno})"))
        .unwrap_or(&text);
    match filename {
        Some(name) => format!(
            "{class}: [Errno {errno}] {text}: {}",
            repr(&name.to_string_lossy())
        ),
        None => format!("{class}: [Errno {errno}] {text}"),
    }
}

/// What a command left: its window of output, its code and whether it overflowed to a file.
struct Finished {
    rendered: String,
    exit_code: i64,
    end: End,
    /// The spill file and the count of characters the command wrote.
    spill: Option<(PathBuf, usize)>,
}

impl Finished {
    fn into_output(self, command: &str, redactor: Redactor) -> Output {
        let output = scrub(&self.rendered, command, true, redactor);
        let mut fields = vec![
            ("output", json_str(&output)),
            ("exit_code", self.exit_code.to_string()),
            ("error", "null".to_owned()),
        ];
        if let Some((path, total)) = self.spill {
            let rewritten = fs::read(&path).and_then(|raw| {
                let raw = String::from_utf8_lossy(&raw);
                collector::rewrite_private(&path, &scrub(&raw, command, false, redactor))
            });
            if rewritten.is_ok() {
                let shown = path.display().to_string();
                fields.push(("output_total_chars", total.to_string()));
                fields.push(("full_output_path", json_str(&shown)));
                fields.push((
                    "truncation_note",
                    json_str(&format!(
                        "Output exceeded the capture window (head+tail shown). Full output ({} \
                         chars) saved to {shown} \u{2014} search it with search_files or page it \
                         with read_file instead of re-running the command.",
                        thousands(total)
                    )),
                ));
            } else {
                // A spill that could not be cleaned is not handed out.
                let _ = fs::remove_file(&path);
            }
        }
        if let Some(note) = exit_note::interpret(command, self.exit_code) {
            fields.push(("exit_code_meaning", json_str(&note)));
        }
        Output {
            content: json_object(fields),
            is_error: self.end != End::Exited,
        }
    }
}

/// The text the model (`trim`: without surrounding whitespace) or the spill file keeps of raw
/// command output: ANSI escapes stripped, then secrets redacted for `command`.
fn scrub(raw: &str, command: &str, trim: bool, redactor: Redactor) -> String {
    let text = ansi::strip_ansi(raw);
    let text = if trim {
        text.trim_matches(is_py_space)
    } else {
        &text
    };
    redactor.terminal_output(text, command).into_owned()
}

/// The shell commands run in: `bash` on `PATH`, else `/usr/bin/bash`, `/bin/bash`, `$SHELL`,
/// `/bin/sh`.
fn find_bash() -> PathBuf {
    let executable = |path: &Path| {
        fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    std::env::var_os("PATH")
        .and_then(|dirs| {
            std::env::split_paths(&dirs)
                .map(|dir| dir.join("bash"))
                .find(|path| executable(path))
        })
        .or_else(|| {
            ["/usr/bin/bash", "/bin/bash"]
                .into_iter()
                .map(PathBuf::from)
                .find(|path| path.is_file())
        })
        .or_else(|| {
            std::env::var_os("SHELL")
                .filter(|shell| !shell.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

/// Runs the command; a failure is Python's `str()` of the `OSError` it maps to, naming `cwd`
/// when the shell could not start in it.
fn execute(
    command: &str,
    timeout: u64,
    cwd: &Path,
    spill_dir: &Path,
    interrupt: &Interrupt,
) -> Result<Finished, String> {
    let os_error = |e: io::Error| python_os_error(&e, None);
    let (reader, writer) = io::pipe().map_err(os_error)?;
    let mut child = Command::new(find_bash())
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(writer.try_clone().map_err(os_error)?)
        .stderr(writer)
        .process_group(0)
        .spawn()
        .map_err(|e| python_os_error(&e, Some(cwd)))?;
    let group = Pid::from_child(&child);
    let spill_path = collector::spill_path(spill_dir, child.id());
    let collector = Arc::new(Mutex::new(Collector::new(
        MAX_OUTPUT_CHARS,
        Some(spill_path),
    )));
    let (events, inbox) = mpsc::channel();

    let deadline = Instant::now() + Duration::from_secs(timeout);
    let waiter = start_threads(reader, group, &collector, &events)
        .inspect_err(|_| {
            // ESRCH only: every member already exited.
            let _ = kill_process_group(group, Signal::KILL);
            let _ = child.wait();
        })
        .map_err(os_error)?;
    let end = if interrupt.attach(events) {
        wait_for_end(&inbox, deadline)
    } else {
        End::Interrupted
    };
    interrupt.detach();
    match end {
        // ESRCH only: every member already exited. The leader is not reaped yet, so the group
        // id cannot name another group.
        End::Exited => {
            let _ = kill_process_group(group, Signal::KILL);
        }
        End::TimedOut | End::Interrupted => terminate(group, &mut child),
    }
    let _ = waiter.join();
    let status = child.wait().map_err(os_error)?;
    drain(&inbox);

    let (exit_code, suffix) = match end {
        End::Exited => (
            status
                .code()
                .or_else(|| status.signal().map(|s| -s))
                .map_or(-1, i64::from),
            String::new(),
        ),
        End::TimedOut => (124, format!("\n[Command timed out after {timeout}s]")),
        End::Interrupted => (130, "\n[Command interrupted]".to_owned()),
    };
    let mut collector = lock(&collector);
    let rendered = collector.render(&suffix);
    let total = collector.total_chars();
    let spill = collector.close_spill().map(|path| (path, total));
    Ok(Finished {
        rendered,
        exit_code,
        end,
        spill,
    })
}

/// Starts the output reader and the exit waiter; returns the waiter to join after the kill.
fn start_threads(
    mut reader: PipeReader,
    group: Pid,
    collector: &Arc<Mutex<Collector>>,
    events: &Sender<Event>,
) -> io::Result<thread::JoinHandle<()>> {
    let sink = Arc::clone(collector);
    let eof = events.clone();
    thread::Builder::new()
        .name("terminal-output".into())
        .spawn(move || {
            let mut decoder = Utf8Decoder::default();
            let mut chunk = [0u8; CHUNK];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        let text = decoder.decode(&chunk[..n]);
                        lock(&sink).append(&text);
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            lock(&sink).append(&decoder.finish());
            let _ = eof.send(Event::Eof);
        })?;
    let exited = events.clone();
    thread::Builder::new()
        .name("terminal-wait".into())
        .spawn(move || {
            // NOWAIT leaves the leader a zombie, so its pid, the group id, stays reserved until
            // the supervisor reaps it.
            let options = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT;
            while let Err(Errno::INTR) = waitid(WaitId::Pid(group), options) {}
            let _ = exited.send(Event::Exited);
        })
}

/// The first of the leader's exit, an interrupt and the deadline.
fn wait_for_end(inbox: &mpsc::Receiver<Event>, deadline: Instant) -> End {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match inbox.recv_timeout(left) {
            Ok(Event::Eof) => {}
            Ok(Event::Exited) | Err(RecvTimeoutError::Disconnected) => return End::Exited,
            Ok(Event::Interrupted) => return End::Interrupted,
            Err(RecvTimeoutError::Timeout) => return End::TimedOut,
        }
    }
}

/// SIGTERM to the group, SIGKILL when it outlives [`KILL_GRACE`], then [`KILL_WAIT`] for it to
/// go.
fn terminate(group: Pid, child: &mut Child) {
    if kill_process_group(group, Signal::TERM) == Err(Errno::SRCH) {
        return;
    }
    if group_gone(group, child, KILL_GRACE) {
        return;
    }
    if kill_process_group(group, Signal::KILL) == Err(Errno::SRCH) {
        return;
    }
    group_gone(group, child, KILL_WAIT);
}

/// Whether every member of the group exits within `within`.
fn group_gone(group: Pid, child: &mut Child, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    loop {
        // A dead leader that is not reaped keeps the group alive.
        let _ = child.try_wait();
        if test_kill_process_group(group) == Err(Errno::SRCH) {
            return true;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        thread::sleep(GROUP_POLL.min(left));
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

fn lock(collector: &Mutex<Collector>) -> MutexGuard<'_, Collector> {
    collector.lock().unwrap_or_else(PoisonError::into_inner)
}
