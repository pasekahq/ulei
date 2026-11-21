//! A scripted embedder helper: a POSIX sh script that speaks `ulei_garden::embed::pipe` with
//! vectors of length 1, for one text per call. It writes its pid to `<name>.pid`, its ready line
//! from the file `identity` and every request line to `<name>.log`, all in its directory.

// Each test crate compiles this module and uses only a subset of it.
#![allow(dead_code)]

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use ulei_garden::embed::pipe::{Helper, StartError};

/// Answers the call with the one vector `[1.0]`.
pub(crate) const ANSWER: &str = r#"printf '{"count":1,"dim":1}\n\000\000\200\077'"#;
/// Never answers the call.
pub(crate) const HANG: &str = "exec sleep 30";

/// Answers `n` calls, then exits on the next one without answering.
pub(crate) fn exits_after(n: usize) -> String {
    format!("calls=$((calls + 1)); [ \"$calls\" -gt {n} ] && exit 0; {ANSWER}")
}

/// Sleeps `seconds` before answering each call.
pub(crate) fn slow(seconds: &str) -> String {
    format!("sleep {seconds}; {ANSWER}")
}

/// Writes the ready line the helpers of `dir` send from their next start: both digests made of
/// `digit`.
pub(crate) fn set_identity(dir: &Path, digit: char) {
    let digest: String = std::iter::repeat_n(digit, 64).collect();
    let line = format!(
        "{{\"name\":\"scripted\",\"model_sha256\":\"{digest}\",\"tokenizer_sha256\":\"{digest}\",\"dim\":1}}\n"
    );
    fs::write(dir.join("identity"), line).unwrap();
}

/// The helper script `name` in `dir` that runs `per_call` for every request line.
pub(crate) fn script(dir: &Path, name: &str, per_call: &str) -> PathBuf {
    if !dir.join("identity").exists() {
        set_identity(dir, 'a');
    }
    let path = dir.join(name);
    let pid = dir.join(format!("{name}.pid"));
    let log = dir.join(format!("{name}.log"));
    let identity = dir.join("identity");
    let body = format!(
        "#!/bin/sh\necho $$ > '{}'\ncat '{}'\nwhile IFS= read -r line; do\n  printf '%s\\n' \"$line\" >> '{}'\n  {per_call}\ndone\n",
        pid.display(),
        identity.display(),
        log.display()
    );
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(&path)
        .unwrap()
        .write_all(body.as_bytes())
        .unwrap();
    path
}

/// Starts the script at `path`, each call with `deadline`.
pub(crate) fn start(path: &Path, deadline: Duration) -> Result<Helper, StartError> {
    Helper::start(Command::new(path), Duration::from_secs(30), deadline)
}

/// The request lines the script `name` of `dir` has read, in order.
pub(crate) fn log(dir: &Path, name: &str) -> Vec<String> {
    fs::read_to_string(dir.join(format!("{name}.log")))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The pid the last start of the script `name` of `dir` wrote.
pub(crate) fn pid(dir: &Path, name: &str) -> u32 {
    fs::read_to_string(dir.join(format!("{name}.pid")))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// True while the process `pid` exists, a zombie included.
pub(crate) fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}
