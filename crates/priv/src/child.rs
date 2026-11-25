//! One command of the policy as a child (ADR-0073 section 4): a fixed environment, its own
//! process group, capped output, SIGTERM at the timeout and SIGKILL after a grace.

use std::io::{self, Read};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{ChildStderr, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::process::{Pid, PidfdFlags, Signal, kill_process_group, pidfd_open};

/// The child's whole environment.
pub const ENV: [(&str, &str); 2] = [
    ("PATH", "/usr/sbin:/usr/bin:/sbin:/bin"),
    ("LANG", "C.UTF-8"),
];
/// From SIGTERM to SIGKILL.
pub const KILL_GRACE: Duration = Duration::from_secs(2);
/// How long the pipes may stay open after the child exits (a descendant holding them).
const DRAIN_GRACE: Duration = Duration::from_secs(1);

/// What a finished child left.
#[derive(Debug, Default)]
pub struct Outcome {
    /// Its exit code, when it exited.
    pub exit: Option<i32>,
    /// Its signal, when one killed it.
    pub signal: Option<i32>,
    /// Whether the timeout killed it.
    pub timed_out: bool,
    /// Its stdout, at most the cap.
    pub stdout: Vec<u8>,
    /// Its stderr, at most the cap.
    pub stderr: Vec<u8>,
    /// Whether a stream reached the cap and was cut.
    pub truncated: bool,
}

struct Stream<R> {
    pipe: Option<R>,
    buf: Vec<u8>,
}

impl<R: Read> Stream<R> {
    /// Reads what is there; closes the pipe at EOF or past the cap. Returns whether it cut.
    fn pump(&mut self, cap: usize) -> bool {
        let Some(pipe) = self.pipe.as_mut() else {
            return false;
        };
        let mut chunk = [0u8; 16 * 1024];
        match pipe.read(&mut chunk) {
            Ok(0) => self.pipe = None,
            Ok(n) => {
                let room = cap - self.buf.len();
                self.buf.extend_from_slice(&chunk[..n.min(room)]);
                if n > room {
                    // Closing the read end gives the writer EPIPE or SIGPIPE, as `| head -c`.
                    self.pipe = None;
                    return true;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => self.pipe = None,
        }
        false
    }
}

/// Runs `argv` and waits for it.
///
/// # Errors
/// The spawn's error (`exec_failed`), or a failed wait.
pub fn run(argv: &[String], timeout: Duration, cap: usize) -> io::Result<Outcome> {
    let (program, rest) = argv
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty argv"))?;
    let mut child = Command::new(program)
        .args(rest)
        .env_clear()
        .envs(ENV)
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()?;
    let pid = Pid::from_child(&child);
    let pidfd = match pidfd_open(pid, PidfdFlags::empty()) {
        Ok(fd) => fd,
        Err(e) => {
            let _ = kill_process_group(pid, Signal::KILL);
            let _ = child.wait();
            return Err(e.into());
        }
    };
    let mut out: Stream<ChildStdout> = Stream {
        pipe: child.stdout.take(),
        buf: Vec::new(),
    };
    let mut err: Stream<ChildStderr> = Stream {
        pipe: child.stderr.take(),
        buf: Vec::new(),
    };
    let mut outcome = Outcome::default();
    let mut deadline = Instant::now() + timeout;
    let mut signals = [Signal::TERM, Signal::KILL].into_iter();
    let mut exited_at: Option<Instant> = None;
    loop {
        let now = Instant::now();
        if let Some(at) = exited_at {
            if (out.pipe.is_none() && err.pipe.is_none()) || now >= at + DRAIN_GRACE {
                break;
            }
        } else if now >= deadline {
            match signals.next() {
                Some(sig) => {
                    outcome.timed_out = true;
                    let _ = kill_process_group(pid, sig);
                    deadline = now + KILL_GRACE;
                }
                None => deadline = now + Duration::from_secs(3600),
            }
            continue;
        }
        let until = exited_at.map_or(deadline, |at| at + DRAIN_GRACE);
        let wait = Timespec::try_from(until.saturating_duration_since(now)).ok();
        let (ready_exit, ready_out, ready_err) = {
            let mut fds = Vec::with_capacity(3);
            let mut slots = [usize::MAX; 3];
            if exited_at.is_none() {
                slots[0] = fds.len();
                fds.push(PollFd::new(&pidfd, PollFlags::IN));
            }
            if let Some(p) = out.pipe.as_ref() {
                slots[1] = fds.len();
                fds.push(PollFd::new(p, PollFlags::IN));
            }
            if let Some(p) = err.pipe.as_ref() {
                slots[2] = fds.len();
                fds.push(PollFd::new(p, PollFlags::IN));
            }
            match poll(&mut fds, wait.as_ref()) {
                Ok(_) => {}
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(e.into()),
            }
            let hit = |slot: usize| slot != usize::MAX && !fds[slot].revents().is_empty();
            (hit(slots[0]), hit(slots[1]), hit(slots[2]))
        };
        if ready_out {
            outcome.truncated |= out.pump(cap);
        }
        if ready_err {
            outcome.truncated |= err.pump(cap);
        }
        if ready_exit {
            exited_at = Some(Instant::now());
        }
    }
    // The leader is a zombie until the wait, so its id still names the group: no descendant
    // of a command outlives it.
    let _ = kill_process_group(pid, Signal::KILL);
    let status = child.wait()?;
    outcome.exit = status.code();
    outcome.signal = status.signal();
    outcome.stdout = out.buf;
    outcome.stderr = err.buf;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn captures_exit_and_output() {
        let o = run(&argv(&["/bin/echo", "hi"]), Duration::from_secs(5), 100).expect("runs");
        assert_eq!(
            (o.exit, o.signal, o.timed_out, o.truncated),
            (Some(0), None, false, false)
        );
        assert_eq!(o.stdout, b"hi\n");
        let o = run(&argv(&["/usr/bin/env"]), Duration::from_secs(5), 1000).expect("runs");
        let env = String::from_utf8(o.stdout).expect("utf-8");
        let mut lines: Vec<&str> = env.lines().collect();
        lines.sort_unstable();
        assert_eq!(
            lines,
            ["LANG=C.UTF-8", "PATH=/usr/sbin:/usr/bin:/sbin:/bin"]
        );
        let o = run(&argv(&["/bin/pwd"]), Duration::from_secs(5), 100).expect("runs");
        assert_eq!(o.stdout, b"/\n");
        let o = run(&argv(&["/bin/cat"]), Duration::from_secs(5), 100).expect("runs");
        assert_eq!((o.exit, o.stdout.len()), (Some(0), 0), "stdin is /dev/null");
        let o = run(
            &argv(&["/bin/ls", "/nonexistent-path"]),
            Duration::from_secs(5),
            1000,
        )
        .expect("runs");
        assert_eq!(o.exit, Some(2));
        assert_ne!(o.stderr.len(), 0);
    }

    #[test]
    fn cuts_at_the_cap_and_kills_at_the_timeout() {
        let o = run(&argv(&["/usr/bin/yes"]), Duration::from_secs(5), 1000).expect("runs");
        assert!(o.truncated);
        assert_eq!(o.stdout.len(), 1000);
        assert_eq!(
            (o.signal, o.timed_out),
            (Some(13), false),
            "SIGPIPE, as under head -c"
        );
        let started = Instant::now();
        let o = run(
            &argv(&["/bin/sleep", "30"]),
            Duration::from_millis(300),
            100,
        )
        .expect("runs");
        assert!(o.timed_out);
        assert_eq!(o.signal, Some(15));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(run(&argv(&["/nonexistent/program"]), Duration::from_secs(1), 10).is_err());
    }
}
