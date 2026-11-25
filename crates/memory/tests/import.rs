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

use rusqlite::Connection;
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

    fn archive(&self) -> PathBuf {
        self.0.join("data/memory.archive")
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
    found_in(socket, q, "notes")
}

fn found_in(socket: &Path, q: &str, source: &str) -> Vec<String> {
    let (status, body) = call(socket, "search", &json!({"q": q, "source": source}));
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

    // Again, with a conflict, a line that is no PutRequest, one with bad base64, one over the
    // body cap, which goes in parts, and a last line without a newline.
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
         import: 8 lines, 2 stored, 3 duplicate, 3 refused\n"
    );
    assert_eq!(items(&socket), 5);
    assert_eq!(found(&socket, "huge"), ["n-9"]);
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

fn sha(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut out, b| {
            use std::fmt::Write as _;
            let _ = write!(out, "{b:02x}");
            out
        })
}

fn unb64(text: &str) -> Vec<u8> {
    let sextet = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        _ => 63,
    };
    let mut out = Vec::new();
    for group in text.as_bytes().chunks(4) {
        let pad = usize::from(group[3] == b'=') + usize::from(group[2] == b'=');
        let n = group[..4 - pad]
            .iter()
            .fold(0u32, |n, &c| n << 6 | u32::from(sextet(c)))
            << (6 * pad);
        out.extend_from_slice(&n.to_be_bytes()[1..4 - pad]);
    }
    out
}

/// `len` bytes deflate cannot shorten.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x.to_le_bytes()[0]
        })
        .collect()
}

/// The original of `id` read back by ranges of 512 KiB, with the length `read` names.
fn read_back(socket: &Path, id: &str) -> (Vec<u8>, u64) {
    let mut out = Vec::new();
    loop {
        let (status, body) = call(
            socket,
            "read",
            &json!({"id": id, "offset": out.len(), "length": 512 * 1024}),
        );
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
        let range: c10::ReadResponse = serde_json::from_slice(&body).unwrap();
        let bytes = unb64(&range.bytes_b64);
        if bytes.is_empty() {
            return (out, range.len);
        }
        out.extend_from_slice(&bytes);
    }
}

fn count(scratch: &Scratch, table: &str) -> i64 {
    Connection::open(scratch.archive())
        .unwrap()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

#[test]
fn five_mib_originals_go_in_parts_from_bytes_path_and_bytes_b64_and_read_back_whole() {
    let scratch = Scratch::new();
    let _daemon = start(&scratch);
    let socket = scratch.socket();
    let from_file = noise(5 * 1024 * 1024 + 3, 3);
    let from_line = noise(5 * 1024 * 1024 + 1, 5);
    let (file_id, line_id) = (sha(&from_file), sha(&from_line));
    let path = scratch.0.join("library-manual.bin");
    fs::write(&path, &from_file).unwrap();
    let by_path = json!({
        "source": "library",
        "external_id": "lib/manual.bin",
        "kind": "application/octet-stream",
        "text": "compass manual",
        "bytes_path": path,
        "id": file_id,
    })
    .to_string();
    let by_b64 = line("lib/inline.bin", "sextant inline", &from_line);
    // An original under 1 MiB whose body with its text is over 1 MiB.
    let wordy = line(
        "lib/wordy.txt",
        &format!("astrolabe {}", "word ".repeat(120_000)),
        &noise(600 * 1024, 9),
    );
    assert!(wordy.len() > MAX_BODY);
    let input = format!("{by_path}\n{by_b64}\n{wordy}\n");

    let out = import(&scratch, &[], input.as_bytes());
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        stderr(&out),
        "import: 3 lines, 3 stored, 0 duplicate, 0 refused\n"
    );
    assert_eq!(items(&socket), 3);
    for (id, original) in [(&file_id, &from_file), (&line_id, &from_line)] {
        let (back, len) = read_back(&socket, id);
        assert_eq!(len, original.len() as u64);
        assert_eq!(sha(&back), *id);
    }
    assert_eq!(found_in(&socket, "compass", "library"), ["lib/manual.bin"]);
    assert_eq!(found(&socket, "sextant"), ["lib/inline.bin"]);
    assert_eq!(found(&socket, "astrolabe"), ["lib/wordy.txt"]);
    assert_eq!(count(&scratch, "uploads"), 0);

    // Again: every line is a duplicate, settled before any part is sent.
    let parts = count(&scratch, "parts");
    let out = import(&scratch, &[], input.as_bytes());
    assert_eq!(
        stderr(&out),
        "import: 3 lines, 0 stored, 3 duplicate, 0 refused\n"
    );
    assert_eq!((items(&socket), count(&scratch, "parts")), (3, parts));

    // The owner's delete removes the item with its parts.
    let deleted = Command::new(MEMORY)
        .args(["delete", &file_id, "--reason", "synthetic", "--config"])
        .arg(scratch.config())
        .output()
        .unwrap();
    assert!(deleted.status.success(), "{}", stderr(&deleted));
    assert_eq!(items(&socket), 2);
    assert_eq!(count(&scratch, "parts"), parts - 11);
    assert_eq!(count(&scratch, "chunked"), 2);
}

#[test]
fn a_wrong_id_or_a_missing_file_refuses_the_line_and_stores_nothing() {
    let scratch = Scratch::new();
    let _daemon = start(&scratch);
    let original = noise(2 * 1024 * 1024, 13);
    let path = scratch.0.join("atlas.bin");
    fs::write(&path, &original).unwrap();
    let wrong = json!({
        "source": "library",
        "external_id": "lib/atlas.bin",
        "kind": "application/octet-stream",
        "text": "atlas",
        "bytes_path": path,
        "id": sha(b"something else"),
    })
    .to_string();
    let missing = json!({
        "source": "library",
        "external_id": "lib/missing.bin",
        "kind": "application/octet-stream",
        "text": "missing",
        "bytes_path": scratch.0.join("missing.bin"),
    })
    .to_string();
    let out = import(&scratch, &[], format!("{wrong}\n{missing}\n").as_bytes());
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let err = stderr(&out);
    let lines: Vec<&str> = err.lines().collect();
    assert_eq!(lines.len(), 3, "{err}");
    assert!(
        lines[0].starts_with("import: line 1: refused invalid_request: the original's SHA-256 is "),
        "{err}"
    );
    assert!(
        lines[1].starts_with("import: line 2: refused invalid_request: bytes_path "),
        "{err}"
    );
    assert_eq!(
        lines[2],
        "import: 2 lines, 0 stored, 0 duplicate, 2 refused"
    );
    assert_eq!(items(&scratch.socket()), 0);
    assert_eq!(count(&scratch, "uploads"), 0);
}
