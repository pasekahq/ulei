//! The `ulei-inbox serve` daemon as a child process and raw C1 requests on its socket.

use std::fs;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};

use serde::de::DeserializeOwned;
use serde_json::Value;
use ulei_core::inbox::canonical;

/// A running daemon, killed on drop.
pub(crate) struct Daemon {
    pub(crate) child: Child,
    pub(crate) socket: PathBuf,
}

impl Daemon {
    /// Kills the daemon and returns what it wrote to stderr.
    pub(crate) fn stderr(mut self) -> String {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let mut stderr = String::new();
        self.child
            .stderr
            .take()
            .expect("stderr")
            .read_to_string(&mut stderr)
            .expect("stderr");
        stderr
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub(crate) enum Started {
    Ready(Daemon),
    Exited(ExitStatus, String),
}

/// Runs `exe <prefix> serve --root <root> --socket <socket>`, with `--deliver <engine>` when
/// given, then `tail`, until its ready line or exit.
pub(crate) fn launch(
    exe: &str,
    prefix: &[&str],
    root: &Path,
    socket: &Path,
    deliver: Option<&Path>,
    tail: &[&str],
) -> Started {
    let mut command = Command::new(exe);
    command
        .args(prefix)
        .arg("serve")
        .arg("--root")
        .arg(root)
        .arg("--socket")
        .arg(socket);
    if let Some(engine) = deliver {
        command.arg("--deliver").arg(engine);
    }
    let mut child = command
        .args(tail)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut line = String::new();
    BufReader::new(child.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("read ready line");
    if line.is_empty() {
        let status = child.wait().expect("wait");
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("stderr")
            .read_to_string(&mut stderr)
            .expect("stderr");
        return Started::Exited(status, stderr);
    }
    assert_eq!(line, format!("ulei-inbox ready {}\n", socket.display()));
    Started::Ready(Daemon {
        child,
        socket: socket.to_owned(),
    })
}

pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) body: Vec<u8>,
}

/// Reads to the end of the connection. Linux reports `ECONNRESET` after the queued answer
/// when the server closed with request bytes unread, so that ends the stream too.
pub(crate) fn read_all(stream: &mut UnixStream) -> Vec<u8> {
    let mut out = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return out,
            Ok(n) => out.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == ErrorKind::ConnectionReset && !out.is_empty() => return out,
            Err(e) => panic!("read: {e}"),
        }
    }
}

/// Parses one answer and checks the framing every answer shares.
pub(crate) fn parse(raw: &[u8]) -> Response {
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("head end");
    let head = std::str::from_utf8(&raw[..end]).expect("utf-8 head");
    let body = raw[end + 4..].to_vec();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().expect("status line");
    let status = status_line
        .strip_prefix("HTTP/1.1 ")
        .and_then(|rest| rest.get(..3))
        .and_then(|code| code.parse().ok())
        .expect("status");
    let headers: Vec<&str> = lines.collect();
    assert_eq!(
        headers,
        [
            "Content-Type: application/json".to_owned(),
            format!("Content-Length: {}", body.len()),
            "Connection: close".to_owned(),
        ],
        "{status_line}"
    );
    Response { status, body }
}

pub(crate) fn exchange(socket: &Path, raw: &[u8]) -> Response {
    let mut stream = UnixStream::connect(socket).expect("connect");
    stream.write_all(raw).expect("write");
    parse(&read_all(&mut stream))
}

pub(crate) fn post_raw(socket: &Path, op: &str, body: &[u8]) -> Response {
    let mut raw = format!(
        "POST /v1/{op} HTTP/1.1\r\nHost: inbox\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    raw.extend_from_slice(body);
    exchange(socket, &raw)
}

/// Posts `body`, expects `status` and returns the answer parsed as the C1 definition `T`.
pub(crate) fn call<T: DeserializeOwned>(socket: &Path, op: &str, body: &Value, status: u16) -> T {
    let response = post_raw(socket, op, &serde_json::to_vec(body).expect("json"));
    assert_eq!(
        response.status,
        status,
        "{op}: {}",
        String::from_utf8_lossy(&response.body)
    );
    c1(&response.body)
}

/// `body` as the C1 definition `T`, checked to be canonical JSON.
pub(crate) fn c1<T: DeserializeOwned>(body: &[u8]) -> T {
    let value: Value = serde_json::from_slice(body).expect("json body");
    assert!(value.is_object(), "not an object");
    assert_eq!(canonical(&value).expect("canonical"), body, "not canonical");
    serde_json::from_value(value).expect("C1 definition")
}

/// The value of `name` in `/proc/<pid>/status`.
pub(crate) fn status_field(pid: u32, name: &str) -> String {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).expect("proc status");
    status
        .lines()
        .find_map(|line| line.strip_prefix(name))
        .expect("status field")
        .trim()
        .to_owned()
}
