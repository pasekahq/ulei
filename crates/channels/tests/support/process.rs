//! Stopping a spawned daemon and reading what it left.

use std::fs;
use std::path::Path;
use std::process::{Child, Command, Output};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Sends SIGTERM to `child`.
pub(crate) fn sigterm(child: &Child) {
    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("kill");
    assert!(status.success());
}

/// The output of `child` once it exits; panics after five seconds.
pub(crate) fn finish(child: Child) -> Output {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    rx.recv_timeout(Duration::from_secs(5))
        .expect("the child did not exit")
        .expect("wait")
}

pub(crate) fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The pid a shell wrote with a trailing newline into the file at `path`, waiting at most 5 s.
pub(crate) fn pid_file(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(text) = fs::read_to_string(path)
            && text.ends_with('\n')
        {
            return text.trim().to_owned();
        }
        assert!(Instant::now() < deadline, "no pid in {}", path.display());
        thread::sleep(Duration::from_millis(5));
    }
}

/// The process `pid` is gone or a zombie waiting for its reaper, waiting at most 1 s.
pub(crate) fn dead(pid: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat"));
        let gone = stat.as_ref().map_or(true, |s| {
            s.rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
        });
        if gone || Instant::now() > deadline {
            return gone;
        }
        thread::sleep(Duration::from_millis(10));
    }
}
