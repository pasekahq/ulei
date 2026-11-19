//! Stopping a spawned daemon and reading what it left.

use std::process::{Child, Command, Output};
use std::sync::mpsc;
use std::time::Duration;

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
