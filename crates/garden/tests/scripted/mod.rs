//! A scripted embedder helper: a POSIX sh script that speaks `ulei_garden::embed::pipe` with
//! vectors of length 1, for one text per call. It writes its pid to `<name>.pid`, its ready line
//! from the file `identity` and every request line to `<name>.log`, all in its directory.

// Each test crate compiles this module and uses only a subset of it.
#![allow(dead_code)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use ulei_garden::embed::pipe::{Helper, StartError};

/// Answers the call with the one vector `[1.0]`.
pub(crate) const ANSWER: &str = r#"printf '{"count":1,"dim":1}\n\000\000\200\077'"#;
/// Answers the call with the one vector `[-1.0]`.
pub(crate) const OPPOSITE: &str = r#"printf '{"count":1,"dim":1}\n\000\000\200\277'"#;
/// Never answers the call.
pub(crate) const HANG: &str = "exec sleep 30";

/// Answers `n` calls, then exits on the next one without answering.
pub(crate) fn exits_after(n: usize) -> String {
    format!("calls=$((calls + 1)); [ \"$calls\" -gt {n} ] && exit 0; {ANSWER}")
}

/// Answers `[1.0]` for a request that holds one of `words` and `[-1.0]` for any other, so the
/// texts with those words land on each other and opposite every other text.
pub(crate) fn close_when(words: &[&str]) -> String {
    let patterns: Vec<String> = words.iter().map(|w| format!("*{w}*")).collect();
    format!(
        "case \"$line\" in {}) {ANSWER} ;; *) {OPPOSITE} ;; esac",
        patterns.join("|")
    )
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

/// Writes the ready line of a helper with lanes (ADR-0087 section 9), `"lanes": 2`, for the
/// helpers of `dir`: both digests made of `a`.
pub(crate) fn set_lanes_identity(dir: &Path) {
    let digest: String = std::iter::repeat_n('a', 64).collect();
    let line = format!(
        "{{\"name\":\"scripted\",\"model_sha256\":\"{digest}\",\"tokenizer_sha256\":\"{digest}\",\"dim\":1,\"lanes\":2}}\n"
    );
    fs::write(dir.join("identity"), line).unwrap();
}

/// For a helper with lanes: sets `id` to the request's id (the requests' keys are in order, so
/// the first `"id":` is the request's own).
pub(crate) const ID: &str = r#"id=${line#*\"id\":}; id=${id%%,*}"#;

/// For a helper with lanes: answers the request `$id` with the one vector `[1.0]`.
pub(crate) const ANSWER_ID: &str =
    r#"printf '{"id":%s,"count":1,"dim":1}\n\000\000\200\077' "$id""#;

/// For a helper with lanes: runs `passage` for a passage and `query` for every other request,
/// each with `$id` set.
pub(crate) fn by_lane(passage: &str, query: &str) -> String {
    format!("{ID}; case \"$line\" in *'\"kind\":\"passage\"'*) {passage} ;; *) {query} ;; esac")
}

/// For a helper with lanes: answers `$id` after `seconds` in the background, so the next
/// request line is read at once.
pub(crate) fn later(seconds: &str) -> String {
    format!("(sleep {seconds}; {ANSWER_ID}) &")
}

/// For a helper with lanes: takes the request and never answers it, in the background and
/// without holding the helper's stdout, so the pipe closes when the helper itself ends.
pub(crate) const SWALLOW: &str = "(exec >/dev/null; sleep 3) &";

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
    // A child forked by another test thread inherits every open descriptor until its exec, so a
    // script written through one in this process can fail to start with ETXTBSY; a separate
    // writer keeps the write descriptor out of this process.
    let mut writer = Command::new("/bin/sh")
        .args([
            "-c",
            "set -C; umask 077; cat > \"$1\" && chmod 700 \"$1\"",
            "sh",
        ])
        .arg(&path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    writer
        .stdin
        .take()
        .unwrap()
        .write_all(body.as_bytes())
        .unwrap();
    assert!(writer.wait().unwrap().success(), "{}", path.display());
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

/// Kills the process `pid`, a child of this test process, and waits until it is a zombie, so
/// its exit is there to see before anything asks it again.
pub(crate) fn kill_idle(pid: u32) {
    let killed = Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let stat = format!("/proc/{pid}/stat");
    let since = std::time::Instant::now();
    while fs::read_to_string(&stat).is_ok_and(|s| !s.contains(") Z ")) {
        assert!(since.elapsed() < Duration::from_secs(3), "{pid} still runs");
        std::thread::sleep(Duration::from_millis(5));
    }
}
