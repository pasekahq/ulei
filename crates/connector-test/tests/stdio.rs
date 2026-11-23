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
