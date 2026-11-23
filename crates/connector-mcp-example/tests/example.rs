//! The example over its MCP seam, wrapping `ulei-connector-mcp-synthetic`: C11 `ulei_pull` calls
//! in, `Page`s or `Error`s out.

use std::io::{BufRead, BufReader, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

const EXAMPLE: &str = env!("CARGO_BIN_EXE_ulei-connector-mcp-example");
const SYNTHETIC: &str = env!("CARGO_BIN_EXE_ulei-connector-mcp-synthetic");

/// The argv wrapping the synthetic server's `list_records`, the server started with `server`.
fn numbered(server: &[&str]) -> Vec<String> {
    let mut argv: Vec<String> = [
        "--tool",
        "list_records",
        "--entries",
        "records",
        "--id",
        "id",
        "--text",
        "body",
        "--cursor",
        "seq",
        "--after",
        "after",
        "--limit",
        "limit",
        "--",
        SYNTHETIC,
    ]
    .map(str::to_owned)
    .to_vec();
    argv.extend(server.iter().map(|a| (*a).to_owned()));
    argv
}

/// The argv wrapping entries that carry every C11 `Item` field under its own name.
fn as_items(entries: &str) -> Vec<String> {
    let mut argv = numbered(&["--entries", entries]);
    let at = argv.iter().position(|a| a == "--id").expect("--id");
    argv.splice(
        at..at + 4,
        [
            "--id",
            "external_id",
            "--text",
            "text",
            "--who",
            "who",
            "--at",
            "at",
            "--kind",
            "kind",
            "--bytes",
            "bytes_b64",
        ]
        .map(str::to_owned),
    );
    let cursor = argv.iter().position(|a| a == "--cursor").expect("--cursor");
    "external_id".clone_into(&mut argv[cursor + 1]);
    argv
}

struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    last_id: u64,
}

impl Session {
    fn start(argv: &[String]) -> Self {
        Self::spawn(
            Command::new(EXAMPLE)
                .args(argv)
                .env("PATH", "/usr/bin:/bin"),
        )
    }

    fn spawn(command: &mut Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("the example starts");
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let mut session = Self {
            child,
            stdin,
            stdout,
            last_id: 0,
        };
        let init = session.call(
            "initialize",
            &json!({"protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": {"name": "mcp", "version": "0.1.0"}}),
        );
        assert_eq!(init["result"]["protocolVersion"], "2025-11-25", "{init}");
        session.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        session
    }

    fn send(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{message}").expect("write");
    }

    /// The response to request `method` with `params`.
    fn call(&mut self, method: &str, params: &Value) -> Value {
        self.last_id += 1;
        let id = self.last_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read");
        let answer: Value = serde_json::from_str(&line).expect("a JSON line");
        assert_eq!(answer["id"], id, "{answer}");
        answer
    }

    /// The `structuredContent` of `ulei_pull` with `request`, and whether it is an error.
    fn pull(&mut self, request: &Value) -> (Value, bool) {
        let answer = self.call(
            "tools/call",
            &json!({"name": "ulei_pull", "arguments": request}),
        );
        let result = &answer["result"];
        (
            result["structuredContent"].clone(),
            result["isError"] == true,
        )
    }

    /// Closes stdin and waits: the example ends with its session.
    fn end(mut self) {
        drop(self.stdin.take());
        let status = self.child.wait().expect("wait");
        assert!(status.success(), "{status}");
    }
}

fn request(cursor: Option<&str>, limit: u64) -> Value {
    json!({
        "contract": "0.1.0",
        "cursor": cursor,
        "limit": limit,
        "key_file": null,
        "data_dir": "/nonexistent/data",
    })
}

fn fixture(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../contracts/fixtures/c11")
        .join(name);
    serde_json::from_slice(&std::fs::read(&path).expect("fixture")).expect("fixture JSON")
}

/// A JSON file in a directory of its own under the temp dir, removed on drop.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Some(dir) = self.0.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// `value` in the file `name`.
fn file(name: &str, value: &Value) -> Scratch {
    let dir = std::env::temp_dir().join(format!(
        "ulei-connector-mcp-example-{}-{name}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let path = dir.join(format!("{name}.json"));
    std::fs::write(&path, value.to_string()).expect("write");
    Scratch(path)
}

fn path(file: &Scratch) -> &str {
    file.0.to_str().expect("UTF-8 path")
}

#[test]
fn pages_carry_the_test_connectors_items_and_continue_from_the_last_entrys_cursor() {
    let mut session = Session::start(&numbered(&["--count", "5"]));
    let tools = session.call("tools/list", &json!({}));
    assert_eq!(tools["result"]["tools"][0]["name"], "ulei_pull");
    assert_eq!(
        session.call("resources/list", &json!({}))["error"]["code"],
        -32601
    );

    let (page, is_error) = session.pull(&request(None, 2));
    assert!(!is_error, "{page}");
    assert_eq!(
        page,
        json!({"items": [
            {"external_id": "item-1", "kind": "text/plain", "text": "synthetic item 1",
                "bytes_b64": "c3ludGhldGljIGl0ZW0gMQ=="},
            {"external_id": "item-2", "kind": "text/plain", "text": "synthetic item 2",
                "bytes_b64": "c3ludGhldGljIGl0ZW0gMg=="},
        ], "cursor": "2", "more": true})
    );
    let mut cursor = "2".to_owned();
    let mut pages = Vec::new();
    loop {
        let (page, is_error) = session.pull(&request(Some(&cursor), 2));
        assert!(!is_error, "{page}");
        let ids: Vec<&str> = page["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|item| item["external_id"].as_str().expect("id"))
            .collect();
        cursor = page["cursor"].as_str().expect("cursor").to_owned();
        pages.push((ids.join(","), cursor.clone(), page["more"] == true));
        if page["more"] != true {
            break;
        }
    }
    assert_eq!(
        pages,
        [
            ("item-3,item-4".to_owned(), "4".to_owned(), true),
            ("item-5".to_owned(), "5".to_owned(), false),
        ]
    );
    let (page, _) = session.pull(&request(Some("5"), 2));
    assert_eq!(page, json!({"items": [], "cursor": "5", "more": false}));
    session.end();
}

#[test]
fn valid_request_fixtures_get_a_page_and_invalid_ones_bad_request() {
    let empty = file("empty", &json!([]));
    let mut session = Session::start(&numbered(&["--entries", path(&empty)]));
    for name in ["Request.first_run.json", "Request.no_key.json"] {
        let request = fixture(&format!("valid/{name}"));
        let (page, is_error) = session.pull(&request);
        assert!(!is_error, "{name}: {page}");
        assert_eq!(
            page,
            json!({"items": [], "cursor": request["cursor"], "more": false}),
            "{name}"
        );
    }
    for name in [
        "Request.extra_key.json",
        "Request.limit_zero.json",
        "Request.missing_cursor.json",
    ] {
        let (error, is_error) = session.pull(&fixture(&format!("invalid/{name}")));
        assert!(is_error, "{name}: {error}");
        assert_eq!(error["code"], "bad_request", "{name}: {error}");
    }
    session.end();
}

#[test]
fn item_fixtures_map_to_the_same_items_and_invalid_ones_to_internal() {
    for name in ["Item.full.json", "Item.minimal.json"] {
        let item = fixture(&format!("valid/{name}"));
        let entries = file(name, &json!([item]));
        let mut session = Session::start(&as_items(path(&entries)));
        let (page, is_error) = session.pull(&request(None, 100));
        assert!(!is_error, "{name}: {page}");
        assert_eq!(page["items"], json!([item]), "{name}");
        assert_eq!(page["cursor"], item["external_id"], "{name}");
        session.end();
    }

    let mut extra = fixture("invalid/Item.extra_source.json");
    let entries = file("extra_source", &json!([extra]));
    let mut session = Session::start(&as_items(path(&entries)));
    let (page, _) = session.pull(&request(None, 100));
    extra.as_object_mut().expect("object").remove("source");
    assert_eq!(page["items"], json!([extra]));
    session.end();

    for (name, reason) in [
        (
            "Item.bytes_not_base64.json",
            "entry 1: bytes_b64 is not padded standard base64",
        ),
        (
            "Item.external_id_too_long.json",
            "entry 1: external_id is not 1 to 512 bytes",
        ),
        ("Item.missing_text.json", "entry 1: text is not a string"),
        ("Item.negative_at.json", "entry 1: at is not a Unix time"),
    ] {
        let entries = file(name, &json!([fixture(&format!("invalid/{name}"))]));
        let mut session = Session::start(&as_items(path(&entries)));
        let (error, is_error) = session.pull(&request(None, 100));
        assert!(is_error, "{name}: {error}");
        assert_eq!(
            error,
            json!({"code": "internal", "message": reason}),
            "{name}"
        );
        session.end();
    }
}

#[test]
fn an_error_fixture_of_the_listing_tool_passes_on_and_an_invalid_one_is_source_unavailable() {
    for name in ["Error.auth_failed.json", "Error.rate_limited.json"] {
        let error = fixture(&format!("valid/{name}"));
        let errors = file(name, &error);
        let mut session = Session::start(&numbered(&["--error", path(&errors)]));
        let (passed, is_error) = session.pull(&request(None, 100));
        assert!(is_error, "{name}");
        assert_eq!(passed, error, "{name}");
        session.end();
    }
    for name in [
        "Error.missing_message.json",
        "Error.retry_after_zero.json",
        "Error.unknown_code.json",
    ] {
        let errors = file(name, &fixture(&format!("invalid/{name}")));
        let mut session = Session::start(&numbered(&["--error", path(&errors)]));
        let (error, is_error) = session.pull(&request(None, 100));
        assert!(is_error, "{name}");
        assert_eq!(
            error,
            json!({"code": "source_unavailable",
                "message": "list_records answered an error: synthetic error"}),
            "{name}"
        );
        session.end();
    }
}

#[test]
fn each_failure_of_the_wrapped_server_or_its_tool_has_its_error_code() {
    let mut no_tool = numbered(&["--count", "1"]);
    no_tool.drain(0..2);
    let mut missing = numbered(&[]);
    *missing.last_mut().expect("program") = "/nonexistent/server".to_owned();
    let mut misnamed = numbered(&["--count", "1"]);
    misnamed[3] = "rows".to_owned();
    let cases = [
        (no_tool, "internal", "argv lacks --tool".to_owned()),
        (
            missing,
            "internal",
            "/nonexistent/server: No such file or directory (os error 2)".to_owned(),
        ),
        (
            misnamed,
            "internal",
            "list_records answered no array \"rows\"".to_owned(),
        ),
        (
            numbered(&["--fail", "exit"]),
            "source_unavailable",
            "the wrapped server ended its stdout before it answered tools/call".to_owned(),
        ),
        (
            numbered(&["--fail", "garbage"]),
            "source_unavailable",
            "the wrapped server's answer to tools/call is not JSON: expected ident at line 1 column 2"
                .to_owned(),
        ),
        (
            numbered(&["--fail", "rpc-error"]),
            "source_unavailable",
            "the wrapped server answered tools/call with JSON-RPC error -32603: \"synthetic failure\""
                .to_owned(),
        ),
        (
            numbered(&["--fail", "tool-error"]),
            "source_unavailable",
            "list_records answered an error: upstream answered 503".to_owned(),
        ),
    ];
    for (argv, code, message) in cases {
        let mut session = Session::start(&argv);
        let (error, is_error) = session.pull(&request(None, 100));
        assert!(is_error, "{argv:?}");
        assert_eq!(error, json!({"code": code, "message": message}), "{argv:?}");
        session.end();
    }
}

#[test]
fn the_wrapped_server_gets_a_cleared_environment_and_not_the_broker_fd() {
    // The shell moves the socket from its stderr to fd 3 and keeps its pid across exec.
    let spawn = |listen_pid: &str| {
        let (_runner, broker) = UnixStream::pair().expect("pair");
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                &format!(
                    r#"LISTEN_PID={listen_pid} LISTEN_FDS=1 LISTEN_FDNAMES=broker exec "$0" "$@" 3<&2 2>/dev/null"#
                ),
                EXAMPLE,
            ])
            .args(numbered(&["--report"]))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", "/nonexistent/home")
            .env("SYNTHETIC_TOKEN", "synthetic-secret")
            .stderr(Stdio::from(OwnedFd::from(broker)));
        let mut session = Session::spawn(&mut command);
        let (page, is_error) = session.pull(&request(None, 100));
        assert!(!is_error, "{page}");
        session.end();
        page["items"][0]["text"]
            .as_str()
            .expect("report")
            .to_owned()
    };
    assert_eq!(spawn("$$"), "env HOME,PATH; fds ");
    let unclaimed = spawn("1");
    assert!(
        unclaimed.starts_with("env HOME,PATH; fds 3 socket:["),
        "{unclaimed}"
    );
}
