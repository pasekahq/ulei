//! `ulei-memory import` through the built binary against a running `ulei-memory serve`: stored
//! and duplicate lines, refusals by the daemon and before it, `--skip`, and a daemon that is not
//! there. All items are synthetic.

#![allow(missing_docs, clippy::unwrap_used)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::json;
use ulei_memory::serve::MAX_BODY;
use ulei_memory::wire::c10;

const MEMORY: &str = env!("CARGO_BIN_EXE_ulei-memory");

/// A fresh 0700 directory short enough for a socket path, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let temp = std::env::temp_dir();
        let base = if temp.as_os_str().len() <= 48 {
            temp
        } else {
            PathBuf::from("/tmp")
        };
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = base.join(format!("ulei-memory-imp-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        Self(dir)
    }

    fn socket(&self) -> PathBuf {
        self.0.join("run/memory.sock")
    }

    /// `ulei.toml` naming the archive, the socket and an engine nothing listens on.
    fn config(&self) -> PathBuf {
        let path = self.0.join("ulei.toml");
        fs::write(
            &path,
            format!(
                "[memory]\narchive = \"{}\"\nsocket = \"{}\"\n\n[engine]\nc6_socket = \"{}\"\n",
                self.0.join("data/memory.archive").display(),
                self.socket().display(),
                self.0.join("engine.sock").display()
            ),
        )
        .unwrap();
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A running daemon, killed on drop.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(scratch: &Scratch) -> Daemon {
    let mut child = Command::new(MEMORY)
        .args(["serve", "--config"])
        .arg(scratch.config())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert!(line.starts_with("ulei-memory ready /"), "{line:?}");
    Daemon(child)
}

/// `ulei-memory import` with `args` and the scratch config, `input` on stdin.
fn import(scratch: &Scratch, args: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(MEMORY)
        .arg("import")
        .args(args)
        .arg("--config")
        .arg(scratch.config())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap();
    out
}

fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).unwrap()
}

/// One `PutRequest` line of the synthetic source `notes`.
fn line(external_id: &str, text: &str, original: &[u8]) -> String {
    json!({
        "source": "notes",
        "external_id": external_id,
        "who": "owner",
        "at": 1_700_000_000.0,
        "kind": "text/plain",
        "text": text,
        "bytes_b64": b64(original),
    })
    .to_string()
}

fn b64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ABC[(n >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// One C10 call on the daemon's socket; the status and body.
fn call(socket: &Path, method: &str, body: &serde_json::Value) -> (u16, Vec<u8>) {
    let body = body.to_string();
    let mut stream = UnixStream::connect(socket).unwrap();
    write!(
        stream,
        "POST /v1/{method} HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let status = std::str::from_utf8(&raw[9..12]).unwrap().parse().unwrap();
    (status, raw[end + 4..].to_vec())
}

fn items(socket: &Path) -> u64 {
    let (status, body) = call(socket, "health", &json!({}));
    assert_eq!(status, 200);
    serde_json::from_slice::<c10::Health>(&body).unwrap().items
}

fn found(socket: &Path, q: &str) -> Vec<String> {
    let (status, body) = call(socket, "search", &json!({"q": q, "source": "notes"}));
    assert_eq!(status, 200);
    let answer: c10::SearchResponse = serde_json::from_slice(&body).unwrap();
    answer
        .hits
        .iter()
        .flat_map(|hit| hit.entries.iter().map(|e| e.external_id.clone()))
        .collect()
}

#[test]
fn lines_are_stored_then_duplicate_and_each_refusal_is_one_line_with_its_number_and_code() {
    let scratch = Scratch::new();
    let _daemon = start(&scratch);
    let socket = scratch.socket();
    let first = [
        line("n-1", "alpha meadow", b"note one"),
        line("n-2", "beta meadow", b"note two"),
        line("n-3", "gamma meadow", b"note three"),
    ]
    .join("\n");
    let out = import(&scratch, &[], format!("{first}\n").as_bytes());
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        stderr(&out),
        "import: 3 lines, 3 stored, 0 duplicate, 0 refused\n"
    );
    assert_eq!(items(&socket), 3);
    assert_eq!(found(&socket, "beta"), ["n-2"]);

    // Again, with a conflict, a line that is no PutRequest, one the daemon refuses, one over the
    // body cap and a last line without a newline.
    let huge = line("n-9", "huge", &vec![b'x'; MAX_BODY]);
    assert!(huge.len() > MAX_BODY);
    let input = [
        first.as_str(),
        &line("n-1", "alpha meadow", b"other bytes"),
        "{\"source\":\"notes\"}",
        &line("n-4", "delta", b"note four").replace("\"bytes_b64\":\"", "\"bytes_b64\":\"!"),
        &huge,
        &line("n-5", "epsilon meadow", b"note five"),
    ]
    .join("\n");
    let out = import(&scratch, &[], input.as_bytes());
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        stderr(&out),
        "import: line 4: refused conflict\n\
         import: line 5: refused invalid_request\n\
         import: line 6: refused invalid_request\n\
         import: line 7: refused too_large\n\
         import: 8 lines, 1 stored, 3 duplicate, 4 refused\n"
    );
    assert_eq!(items(&socket), 4);
    let mut meadow = found(&socket, "meadow");
    meadow.sort();
    assert_eq!(meadow, ["n-1", "n-2", "n-3", "n-5"]);
}

#[test]
fn skip_resumes_after_the_given_line() {
    let scratch = Scratch::new();
    let _daemon = start(&scratch);
    let input = (1..=4)
        .map(|n| {
            line(
                &format!("s-{n}"),
                &format!("river{n}"),
                format!("{n}").as_bytes(),
            ) + "\n"
        })
        .collect::<String>();
    let out = import(&scratch, &["--skip", "2"], input.as_bytes());
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        stderr(&out),
        "import: 2 lines, 2 stored, 0 duplicate, 0 refused\n"
    );
    assert_eq!(
        found(&scratch.socket(), "river1 OR river2"),
        Vec::<String>::new()
    );
    let mut rest = found(&scratch.socket(), "river3 OR river4");
    rest.sort();
    assert_eq!(rest, ["s-3", "s-4"]);
}

#[test]
fn a_daemon_that_is_not_there_stops_with_exit_1_naming_the_line_to_resume_after() {
    let scratch = Scratch::new();
    let input = format!(
        "not json\n{}\n{}\n",
        line("d-1", "x", b"1"),
        line("d-2", "y", b"2")
    );
    let started = Instant::now();
    let out = import(&scratch, &[], input.as_bytes());
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(started.elapsed() >= Duration::from_secs(7), "no pauses");
    let err = stderr(&out);
    let lines: Vec<&str> = err.lines().collect();
    assert_eq!(lines.len(), 3, "{err}");
    assert_eq!(lines[0], "import: line 1: refused invalid_request");
    assert!(
        lines[1].starts_with("import: stopped at line 2: ")
            && lines[1].ends_with("(after 4 tries); lines 1 to 1 are done, resume with --skip 1"),
        "{err}"
    );
    assert_eq!(
        lines[2],
        "import: 1 lines, 0 stored, 0 duplicate, 1 refused"
    );
}

#[test]
fn a_wrong_flag_is_a_usage_error() {
    let scratch = Scratch::new();
    for args in [&["--skip", "x"][..], &["--skip"], &["--force", "1"]] {
        let out = import(&scratch, args, b"");
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(
            stderr(&out).starts_with("usage: ulei-memory serve"),
            "{args:?}"
        );
    }
}
