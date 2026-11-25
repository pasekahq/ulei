//! One run of the hive's CLI: an argv vector, a cleared environment, a timeout and caps on what
//! is read of stdout and stderr.

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// The most bytes of stdout read.
pub const MAX_STDOUT: usize = 4 * 1024 * 1024;
/// The most bytes of stderr read.
pub const MAX_STDERR: usize = 64 * 1024;
/// Variables of the server's environment the CLI keeps; everything else is cleared.
pub const KEPT_ENV: [&str; 5] = [
    "PATH",
    "HOME",
    "LANG",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
];
const POLL: Duration = Duration::from_millis(10);

/// How a CLI run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exit {
    /// The exit status; `None` when a signal ended it.
    pub status: Option<i32>,
    /// Its stdout.
    pub stdout: Vec<u8>,
    /// Its stderr.
    pub stderr: Vec<u8>,
}

/// Why a CLI run gave no [`Exit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// It could not be started.
    Spawn(String),
    /// It ran past the timeout and was killed.
    TimedOut(Duration),
    /// It wrote more than the cap to stdout or stderr.
    TooLong(&'static str),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "the hive CLI could not be started: {e}"),
            Self::TimedOut(t) => write!(f, "the hive CLI did not finish within {} s", t.as_secs()),
            Self::TooLong(stream) => write!(f, "the hive CLI wrote more to {stream} than is read"),
        }
    }
}

/// Runs `command` followed by `args` with `HIVE_CONFIG=hive_config`, waiting at most `timeout`.
///
/// # Errors
/// [`Failure`] when it did not run to an exit within its limits.
pub fn run(
    command: &[String],
    args: &[String],
    hive_config: &Path,
    timeout: Duration,
) -> Result<Exit, Failure> {
    let (program, prefix) = command
        .split_first()
        .ok_or_else(|| Failure::Spawn("empty command".into()))?;
    let mut cmd = Command::new(program);
    cmd.args(prefix)
        .args(args)
        .env_clear()
        .env("HIVE_CONFIG", hive_config)
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in KEPT_ENV {
        if let Some(value) = std::env::var_os(name) {
            cmd.env(name, value);
        }
    }
    let mut child = cmd.spawn().map_err(|e| Failure::Spawn(e.to_string()))?;
    let stdout = child.stdout.take().map(|s| capped(s, MAX_STDOUT));
    let stderr = child.stderr.take().map(|s| capped(s, MAX_STDERR));
    // On a timeout the readers are left to end with the pipes: a grandchild may hold them.
    let status = wait(&mut child, timeout)?;
    let collect = |h: Option<thread::JoinHandle<(Vec<u8>, bool)>>| {
        h.and_then(|h| h.join().ok()).unwrap_or_default()
    };
    let (stdout, out_over) = collect(stdout);
    let (stderr, err_over) = collect(stderr);
    if out_over {
        return Err(Failure::TooLong("stdout"));
    }
    if err_over {
        return Err(Failure::TooLong("stderr"));
    }
    Ok(Exit {
        status,
        stdout,
        stderr,
    })
}

/// The exit status of `child`, killing it at `timeout`.
fn wait(child: &mut Child, timeout: Duration) -> Result<Option<i32>, Failure> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status.code()),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Failure::TimedOut(timeout));
            }
            Ok(None) => thread::sleep(POLL),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Failure::Spawn(e.to_string()));
            }
        }
    }
}

/// A thread reading `stream` to its end, keeping at most `cap` bytes; true when it had more.
fn capped(
    mut stream: impl Read + Send + 'static,
    cap: usize,
) -> thread::JoinHandle<(Vec<u8>, bool)> {
    thread::spawn(move || {
        let mut kept = Vec::new();
        let mut over = false;
        let mut buf = [0u8; 8192];
        loop {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let room = cap.saturating_sub(kept.len());
                    if n > room {
                        over = true;
                    }
                    kept.extend_from_slice(&buf[..n.min(room)]);
                }
            }
        }
        (kept, over)
    })
}
