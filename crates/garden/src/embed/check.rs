//! The daemon's periodic content check of the encoder engine's weight file (ADR-0087 open
//! question 15). The helper's start reads only the preamble and the header of the file
//! (ADR-0087 E6c), so a changed byte in a blob is seen only by hashing the whole file again. The
//! daemon does not hash it itself: it runs `ulei-garden-embed verify FILE` as a child process,
//! which keeps the weight format out of the daemon and the read buffers out of its memory, and
//! tells a corrupted file from every other failure by the child's exit code
//! [`EXIT_CORRUPTED`] alone.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// The exit code of `ulei-garden-embed verify` for a corrupted weight file, and of the helper
/// that refuses to start on one.
pub const EXIT_CORRUPTED: i32 = 3;

/// The exit code of `ulei-garden-embed verify` for a file it cannot open or read or of an older
/// format ("convert the model again").
pub const EXIT_UNUSABLE: i32 = 1;

/// The longest a check may run before its child is killed: a 600 MB file hashes in about 4 s
/// on the pilot host (ADR-0087 E6c), so this leaves room for a slow disk and a loaded host.
pub const DEADLINE: Duration = Duration::from_secs(300);

/// What a check found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The file hashes to the content digest sealed in it; the digest, lowercase hex.
    Good(String),
    /// The file is not the one its converter wrote; the reason, which starts with
    /// "model file corrupted: " and names the file.
    Corrupted(String),
    /// The file is not a weight file this helper can run any more: missing, unreadable or of an
    /// older format (`verify` exit 1). The running helper still maps the file it started on, but
    /// no helper can start on what is there now, so dense goes down until a check passes again
    /// (ADR-0087 E12a, F5). The reason, which starts with "model file unusable: ".
    Unusable(String),
    /// The check did not decide: the helper cannot be run, was killed or missed [`DEADLINE`].
    /// The reason.
    Failed(String),
}

/// A content check the worker runs; tests inject their own.
pub type Check = dyn Fn() -> Verdict + Send;

/// The check of the weight file `file` by `helper verify FILE`.
pub fn helper_verify(helper: PathBuf, file: PathBuf) -> impl Fn() -> Verdict + Send {
    move || verify(&helper, &file, DEADLINE)
}

/// Runs `helper verify file` and waits for it at most `deadline`; a child that misses it is
/// killed and reaped.
pub fn verify(helper: &Path, file: &Path, deadline: Duration) -> Verdict {
    let child = Command::new(helper)
        .arg("verify")
        .arg(file)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) => return Verdict::Failed(format!("{}: cannot start: {e}", helper.display())),
    };
    let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Verdict::Failed("no pipe to the helper".into());
    };
    let (tx, rx) = mpsc::sync_channel(1);
    // The child writes one short line to one of the two pipes, so reading them in turn cannot
    // fill the other; the thread ends when the child's pipes close.
    let reader = thread::Builder::new()
        .name("garden-check".into())
        .spawn(move || {
            let mut out = String::new();
            let mut err = String::new();
            let read = stdout
                .read_to_string(&mut out)
                .and_then(|_| stderr.read_to_string(&mut err));
            let _ = tx.send(read.map(|_| (out, err)));
        });
    if let Err(e) = reader {
        let _ = child.kill();
        let _ = child.wait();
        return Verdict::Failed(format!("check thread: {e}"));
    }
    let Ok(read) = rx.recv_timeout(deadline) else {
        let _ = child.kill();
        let _ = child.wait();
        return Verdict::Failed(format!("verify: no answer within {deadline:?}"));
    };
    let status = child.wait();
    let (out, err) = match read {
        Ok(text) => text,
        Err(e) => return Verdict::Failed(format!("verify: {e}")),
    };
    match status {
        Ok(status) => verdict(status.code(), &out, &err),
        Err(e) => Verdict::Failed(format!("verify: {e}")),
    }
}

/// The verdict of `verify`'s exit `code`, its stdout and its stderr.
fn verdict(code: Option<i32>, out: &str, err: &str) -> Verdict {
    let line = err
        .lines()
        .next()
        .unwrap_or_default()
        .trim_start_matches("ulei-garden-embed verify: ");
    match code {
        Some(0) => match digest_of(out) {
            Some(digest) => Verdict::Good(digest),
            None => Verdict::Failed(format!("verify: unexpected answer {:?}", out.trim_end())),
        },
        Some(EXIT_CORRUPTED) => Verdict::Corrupted(corrupted(line)),
        Some(EXIT_UNUSABLE) => Verdict::Unusable(format!("model file unusable: {line}")),
        Some(code) => Verdict::Failed(format!("verify exited {code}: {line}")),
        None => Verdict::Failed(format!("verify killed by a signal: {line}")),
    }
}

/// The digest of `verify`'s line `OK FILE: content sha256 <hex>, <bytes> bytes, <format>`.
fn digest_of(out: &str) -> Option<String> {
    const MARK: &str = ": content sha256 ";
    let line = out.lines().next()?;
    let at = line.rfind(MARK)? + MARK.len();
    let hex = line.get(at..at + 64)?;
    hex.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        .then(|| hex.to_owned())
}

/// `verify`'s line `FILE: model file corrupted: DETAIL` as the helper's start names a corrupted
/// file: `model file corrupted: FILE: DETAIL`.
fn corrupted(line: &str) -> String {
    const MARK: &str = ": model file corrupted: ";
    match line.split_once(MARK) {
        Some((file, detail)) => format!("model file corrupted: {file}: {detail}"),
        None if line.starts_with("model file corrupted") => line.to_owned(),
        None => format!("model file corrupted: {line}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn exit_codes_decide_the_verdict() {
        let ok = format!("OK /m/model.ulei-infer: content sha256 {HEX}, 10 bytes, f/4\n");
        assert_eq!(verdict(Some(0), &ok, ""), Verdict::Good(HEX.into()));
        assert!(matches!(verdict(Some(0), "OK\n", ""), Verdict::Failed(_)));
        assert_eq!(
            verdict(
                Some(3),
                "",
                "ulei-garden-embed verify: /m/f: model file corrupted: content SHA-256 is a\n"
            ),
            Verdict::Corrupted("model file corrupted: /m/f: content SHA-256 is a".into())
        );
        assert_eq!(
            verdict(
                Some(1),
                "",
                "ulei-garden-embed verify: /m/f: model file corrupted: x\n"
            ),
            Verdict::Unusable("model file unusable: /m/f: model file corrupted: x".into()),
            "only the exit code says corrupted; 1 is an unusable file"
        );
        assert!(matches!(verdict(Some(2), "", "usage"), Verdict::Failed(_)));
        // A line that already starts with the class keeps it once, unchanged.
        assert_eq!(
            verdict(
                Some(3),
                "",
                "ulei-garden-embed verify: model file corrupted: never sealed\n"
            ),
            Verdict::Corrupted("model file corrupted: never sealed".into())
        );
        assert!(matches!(verdict(None, "", ""), Verdict::Failed(_)));
    }

    #[test]
    fn a_missing_helper_fails_the_check_without_a_verdict() {
        let v = verify(
            Path::new("/nonexistent/ulei-garden-embed"),
            Path::new("/nonexistent/model"),
            Duration::from_secs(1),
        );
        assert!(matches!(v, Verdict::Failed(_)), "{v:?}");
    }
}
