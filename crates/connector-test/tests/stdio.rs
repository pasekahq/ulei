//! The connector over its stdio seam: one C11 `Request` line in, C11 `Line`s out.

use std::io::Write;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::json;
use ulei_contracts::generated::c11::{ErrorCode, Line, LineType};

/// `x-stdio.max_line_bytes` of C11.
const MAX_LINE_BYTES: usize = 1_052_672;

struct Output {
    success: bool,
    stdout: String,
    stderr: String,
    lines: Vec<Line>,
}

/// One run of the binary with `request` as its stdin line.
fn run(request: &serde_json::Value) -> Output {
    run_with(&[], request)
}

/// One run of the binary with `args` and `request` as its stdin line.
fn run_with(args: &[&str], request: &serde_json::Value) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ulei-connector-test"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("connector starts");
    let mut stdin = child.stdin.take().expect("stdin is piped");
    writeln!(stdin, "{request}").expect("request is written");
    drop(stdin);
    let out = child.wait_with_output().expect("connector exits");
    let stdout = String::from_utf8(out.stdout).expect("stdout is UTF-8");
    let lines = stdout
        .split_terminator('\n')
        .map(|text| {
            assert!(text.len() <= MAX_LINE_BYTES, "line over max_line_bytes");
            serde_json::from_str::<Line>(text).unwrap_or_else(|e| panic!("{text}: {e}"))
        })
        .collect();
    Output {
        success: out.status.success(),
        stdout,
        stderr: String::from_utf8(out.stderr).expect("stderr is UTF-8"),
        lines,
    }
}

fn request(cursor: Option<&str>, limit: u64, key_file: Option<&str>) -> serde_json::Value {
    json!({
        "contract": "0.1.0",
        "cursor": cursor,
        "limit": limit,
        "key_file": key_file,
        "data_dir": "/nonexistent/data",
    })
}

/// The items' external ids and the checkpoints' cursors of a successful run, in order, after
/// checking that it alternates item and checkpoint and ends with `done` at the last cursor.
fn ids_and_cursors(out: &Output) -> (Vec<String>, Vec<String>) {
    assert!(out.success, "stderr: {}", out.stderr);
    let (done, pages) = out.lines.split_last().expect("a done line");
    assert_eq!(done.r#type, LineType::Done);
    let mut ids = Vec::new();
    let mut cursors = Vec::new();
    for pair in pages.chunks(2) {
        let [item, checkpoint] = pair else {
            panic!("an item without its checkpoint")
        };
        assert_eq!(item.r#type, LineType::Item);
        assert!(item.kind.is_some() && item.text.is_some() && item.bytes_b64.is_some());
        assert_eq!(checkpoint.r#type, LineType::Checkpoint);
        ids.push(item.external_id.clone().expect("item has external_id"));
        cursors.push(checkpoint.cursor.clone().expect("checkpoint has cursor"));
    }
    assert_eq!(done.cursor.as_ref(), cursors.last());
    (ids, cursors)
}

/// A directory under the temp dir, or under `/tmp` when the temp dir path leaves too little of
/// the 107 bytes a socket path may have.
fn scratch(name: &str) -> PathBuf {
    let temp = std::env::temp_dir();
    let base = if temp.as_os_str().len() <= 48 {
        temp
    } else {
        PathBuf::from("/tmp")
    };
    let dir = base.join(format!("ulei-connector-test-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

#[test]
fn a_first_run_writes_limit_items_each_with_a_checkpoint_then_done() {
    let out = run(&request(None, 3, None));
    let (ids, cursors) = ids_and_cursors(&out);
    assert_eq!(ids, ["item-1", "item-2", "item-3"]);
    assert_eq!(cursors, ["1", "2", "3"]);
    assert_eq!(
        out.stdout.lines().next(),
        Some(
            r#"{"type":"item","external_id":"item-1","kind":"text/plain","text":"synthetic item 1","bytes_b64":"c3ludGhldGljIGl0ZW0gMQ=="}"#
        )
    );
}

#[test]
fn a_second_run_from_the_stored_cursor_continues_the_numbering() {
    let first = run(&request(None, 2, None));
    let stored = first.lines.last().and_then(|done| done.cursor.clone());
    let second = run(&request(stored.as_deref(), 4, None));
    let (ids, cursors) = ids_and_cursors(&second);
    assert_eq!(ids, ["item-3", "item-4", "item-5", "item-6"]);
    assert_eq!(cursors, ["3", "4", "5", "6"]);
}

#[test]
fn an_item_carries_the_key_files_sha256_and_never_the_key() {
    let dir = scratch("key");
    let key_file = dir.join("source.env");
    std::fs::write(&key_file, "SYNTHETIC_TOKEN=synthetic-key-value-7f3a\n").expect("key file");
    let key_file = key_file.to_str().expect("UTF-8 path");
    let out = run(&request(Some("10"), 2, Some(key_file)));
    let (ids, _) = ids_and_cursors(&out);
    assert_eq!(ids, ["item-11", "item-12"]);
    let digest = "8889e49c448f2c7487e5484ed3c5f25d198157cf55065707418312ee995c0d6b";
    for item in out.lines.iter().filter(|l| l.r#type == LineType::Item) {
        let text = item.text.as_deref().expect("item text");
        assert!(text.ends_with(&format!("\nkey sha256:{digest}")), "{text}");
    }
    for stream in [&out.stdout, &out.stderr] {
        assert!(!stream.contains("synthetic-key-value-7f3a"));
    }
    std::fs::remove_dir_all(dir).expect("scratch removed");
}

#[test]
fn a_refused_request_ends_with_one_error_line_and_a_failed_exit() {
    let dir = scratch("refused");
    let missing = dir.join("missing.env");
    let missing = missing.to_str().expect("UTF-8 path");
    let cases = [
        (request(Some("seven"), 1, None), ErrorCode::BadCursor),
        (request(None, 1, Some(missing)), ErrorCode::KeyUnreadable),
        (
            json!({"contract": "0.1.0", "limit": 1}),
            ErrorCode::BadRequest,
        ),
    ];
    for (req, code) in cases {
        let out = run(&req);
        assert!(!out.success, "{req}");
        let [line] = out.lines.as_slice() else {
            panic!("{req}: {:?}", out.lines)
        };
        assert_eq!(line.r#type, LineType::Error);
        assert_eq!(line.code, Some(code));
        assert!(line.message.is_some());
    }
    std::fs::remove_dir_all(dir).expect("scratch removed");
}

#[test]
fn probes_put_the_answer_of_each_socket_call_into_an_item_before_the_numbered_ones() {
    let dir = scratch("probes");
    let listening = dir.join("listening.sock");
    let _listener = UnixListener::bind(&listening).expect("bind");
    let missing = dir.join("missing.sock");
    let (listening, missing) = (
        listening.to_str().expect("UTF-8 path"),
        missing.to_str().expect("UTF-8 path"),
    );
    let out = run_with(
        &[
            "--probe-inet",
            "--probe-unix",
            listening,
            "--probe-unix",
            missing,
        ],
        &request(None, 1, None),
    );
    let probes: Vec<(&str, &str)> = out.lines[..3]
        .iter()
        .map(|line| {
            assert_eq!(line.r#type, LineType::Item);
            (
                line.external_id.as_deref().expect("id"),
                line.text.as_deref().expect("text"),
            )
        })
        .collect();
    assert_eq!(
        probes,
        [
            ("probe-inet", "socket(AF_INET): created"),
            (
                "probe-unix",
                format!("connect(AF_UNIX) {listening}: connected").as_str()
            ),
            (
                "probe-unix",
                format!("connect(AF_UNIX) {missing}: ENOENT errno 2").as_str()
            ),
        ]
    );
    let rest = Output {
        lines: out.lines[3..].to_vec(),
        ..out
    };
    assert_eq!(ids_and_cursors(&rest).0, ["item-1"]);

    let out = run_with(&["--probe-unix"], &request(None, 1, None));
    assert!(!out.success);
    assert_eq!(out.lines.len(), 1);
    assert_eq!(out.lines[0].code, Some(ErrorCode::BadRequest));
    std::fs::remove_dir_all(dir).expect("scratch removed");
}

#[test]
fn file_probes_put_the_answer_of_each_file_call_into_an_item() {
    let dir = scratch("file-probes");
    let file = dir.join("file.txt");
    std::fs::write(&file, "abcdef").expect("file");
    let (file, new, missing) = (
        file.to_str().expect("UTF-8 path").to_owned(),
        dir.join("new.txt").to_str().expect("UTF-8 path").to_owned(),
        dir.join("missing.txt")
            .to_str()
            .expect("UTF-8 path")
            .to_owned(),
    );
    let out = run_with(
        &[
            "--probe-read",
            &file,
            "--probe-write",
            &file,
            "--probe-read",
            &file,
            "--probe-create",
            &new,
            "--probe-create",
            &new,
            "--probe-truncate",
            &file,
            "--probe-read",
            &missing,
        ],
        &request(None, 0, None),
    );
    assert!(out.success, "{}", out.stderr);
    let probes: Vec<(&str, &str)> = out.lines[..7]
        .iter()
        .map(|line| {
            (
                line.external_id.as_deref().expect("id"),
                line.text.as_deref().expect("text"),
            )
        })
        .collect();
    let expected = [
        ("probe-read", format!("read {file}: 6 bytes")),
        ("probe-write", format!("write {file}: written")),
        ("probe-read", format!("read {file}: 12 bytes")),
        ("probe-create", format!("create {new}: created")),
        ("probe-create", format!("create {new}: errno 17")),
        ("probe-truncate", format!("truncate {file}: truncated")),
        ("probe-read", format!("read {missing}: ENOENT errno 2")),
    ];
    let expected: Vec<(&str, &str)> = expected.iter().map(|(id, t)| (*id, t.as_str())).collect();
    assert_eq!(probes, expected);
    assert_eq!(std::fs::read(dir.join("file.txt")).expect("file"), b"");

    let out = run_with(&["--probe-truncate"], &request(None, 1, None));
    assert!(!out.success);
    assert_eq!(out.lines[0].code, Some(ErrorCode::BadRequest));
    std::fs::remove_dir_all(dir).expect("scratch removed");
}

/// A broker on `runner` that connects every request for `entry` to `source` and passes the
/// socket, `not_allowed` for any other entry, and `limit` after 32 connections.
fn fake_broker(
    runner: std::os::unix::net::UnixStream,
    entry: String,
    source: std::net::SocketAddr,
) -> std::thread::JoinHandle<Vec<String>> {
    use std::io::{BufRead, BufReader, IoSlice};
    use std::mem::MaybeUninit;
    use std::os::fd::AsFd;

    use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags};

    std::thread::spawn(move || {
        let mut seen = Vec::new();
        let mut used = 0;
        for line in BufReader::new(&runner).lines() {
            let line = line.expect("request line");
            let request: serde_json::Value = serde_json::from_str(&line).expect("request");
            seen.push(line);
            let socket = (request["connect"] == entry.as_str() && used < 32)
                .then(|| std::net::TcpStream::connect(source).expect("source"));
            let reply = match (&socket, request["connect"] == entry.as_str()) {
                (Some(_), _) => format!("{{\"ok\":\"{source}\"}}\n"),
                (None, true) => "{\"error\":\"limit\"}\n".to_owned(),
                (None, false) => "{\"error\":\"not_allowed\"}\n".to_owned(),
            };
            used += usize::from(socket.is_some());
            let fds: Vec<_> = socket.iter().map(|s| s.as_fd()).collect();
            let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
            let mut control = SendAncillaryBuffer::new(&mut space);
            if !fds.is_empty() {
                assert!(control.push(SendAncillaryMessage::ScmRights(&fds)));
            }
            let sent = rustix::net::sendmsg(
                &runner,
                &[IoSlice::new(reply.as_bytes())],
                &mut control,
                SendFlags::NOSIGNAL,
            )
            .expect("sendmsg");
            assert_eq!(sent, reply.len());
        }
        seen
    })
}

#[test]
fn broker_probes_ask_on_fd_3_and_read_the_first_line_through_the_passed_socket() {
    let dir = scratch("broker");
    let source = std::net::TcpListener::bind("127.0.0.1:0").expect("source");
    let address = source.local_addr().expect("address");
    std::thread::spawn(move || {
        for conn in source.incoming() {
            let _ = conn.and_then(|mut c| c.write_all(b"fake source line\nmore\n"));
        }
    });
    let entry = format!("localhost:{}", address.port());
    let (runner, connector) = std::os::unix::net::UnixStream::pair().expect("pair");
    let broker = fake_broker(runner, entry.clone(), address);
    let request_file = dir.join("request.json");
    std::fs::write(&request_file, format!("{}\n", request(None, 1, None))).expect("request");
    // The shell moves the broker from its stdin to fd 3 and keeps its pid across exec.
    let out = Command::new("/bin/sh")
        .args([
            "-c",
            r#"LISTEN_PID=$$ LISTEN_FDS=1 LISTEN_FDNAMES=broker exec "$0" "$@" 3<&0 0<"$REQUEST""#,
            env!("CARGO_BIN_EXE_ulei-connector-test"),
            "--connect",
            &entry,
            "--connect",
            "denied.example.test:443",
            "--probe-limit",
            &entry,
        ])
        .env("REQUEST", &request_file)
        .stdin(Stdio::from(std::os::fd::OwnedFd::from(connector)))
        .output()
        .expect("connector runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lines: Vec<Line> = String::from_utf8(out.stdout)
        .expect("UTF-8")
        .lines()
        .map(|l| serde_json::from_str(l).expect("line"))
        .collect();
    let probes: Vec<(&str, &str)> = lines[..3]
        .iter()
        .map(|l| {
            (
                l.external_id.as_deref().expect("id"),
                l.text.as_deref().expect("text"),
            )
        })
        .collect();
    assert_eq!(
        probes,
        [
            (
                "probe-connect",
                format!("connect {entry}: {{\"ok\":\"{address}\"}}; first line: fake source line")
                    .as_str()
            ),
            (
                "probe-connect",
                "connect denied.example.test:443: {\"error\":\"not_allowed\"}"
            ),
            (
                "probe-limit",
                format!("limit {entry}: 31 ok, then {{\"error\":\"limit\"}}").as_str()
            ),
        ]
    );
    let seen = broker.join().expect("broker");
    assert_eq!(seen.len(), 34);
    assert_eq!(seen[1], r#"{"connect":"denied.example.test:443"}"#);

    let out = run_with(&["--connect", &entry], &request(None, 1, None));
    assert!(out.success, "{}", out.stderr);
    assert_eq!(
        out.lines[0].text.as_deref(),
        Some(
            format!("connect {entry}: no broker: LISTEN_PID: environment variable not found")
                .as_str()
        )
    );
    std::fs::remove_dir_all(dir).expect("scratch removed");
}

#[test]
fn the_direct_probe_opens_its_own_socket_and_connects_to_the_given_port() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
    let open = listener.local_addr().expect("address").to_string();
    let out = run_with(
        &["--probe-direct", &open, "--probe-direct", "[::1]:9"],
        &request(None, 0, None),
    );
    assert!(out.success, "{}", out.stderr);
    assert_eq!(out.lines[0].external_id.as_deref(), Some("probe-direct"));
    assert_eq!(
        out.lines[0].text.as_deref(),
        Some(format!("direct {open}: socket(AF_INET) created, connected").as_str())
    );
    let v6 = out.lines[1].text.as_deref().expect("text");
    assert!(v6.starts_with("direct [::1]:9: socket(AF_INET6)"), "{v6}");

    let out = run_with(&["--probe-direct", "localhost:80"], &request(None, 1, None));
    assert!(!out.success);
    assert_eq!(out.lines[0].code, Some(ErrorCode::BadRequest));
}
