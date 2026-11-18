//! `ulei-engine serve` through the built binary and raw bytes on its C6 socket, its turns on
//! the fake gateway; the peer check, the request deadline and the kept reports through
//! `ulei_engine::serve::Server` on a socketpair.

#![allow(missing_docs, clippy::unwrap_used)]

mod fake_gateway;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fake_gateway::{
    Answer, FakeGateway, Scratch, gateway_error, route, stop, text, text_step, turns, usage,
};
use serde_json::{Value, json};
use ulei_contracts::generated::c4g::{ErrorClass, StopReason};
use ulei_contracts::generated::c6::{
    Error as WireError, ErrorCode, IdleStatus, SessionStatus, SubmitTurnRequest,
    SubmitTurnResponse, TurnOutcome, TurnReport, TurnState,
};
use ulei_engine::journal::Journal;
use ulei_engine::serve::{Ending, KEPT_REPORTS, MAX_QUEUE, READ_TIMEOUT, Runner, Server};

const BIN: &str = env!("CARGO_BIN_EXE_ulei-engine");
const OWNER: &str = "owner";

/// A running `ulei-engine serve`, killed on drop.
struct Engine {
    child: Child,
    socket: PathBuf,
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The settings of a serve run in `dir` against `gateway`, its C6 socket at `socket`.
fn config(dir: &Path, gateway: &Path, socket: &Path) -> PathBuf {
    let brain = dir.join("brain");
    fs::create_dir_all(&brain).unwrap();
    fs::write(brain.join("SOUL.md"), "You are the test assistant.\n").unwrap();
    let text = format!(
        "[qb]\nsocket = \"{}\"\n\n[engine]\njournal = \"{}\"\nterminal_output = \"{}\"\nsoul_file = \"{}\"\nindex_file = \"{}\"\nmemory_file = \"{}\"\nuser_file = \"{}\"\nskills_dir = \"{}\"\nc6_socket = \"{}\"\nowner_session = \"{OWNER}\"\ndefault_route = \"main\"\n",
        gateway.display(),
        dir.join("journal.sqlite3").display(),
        dir.join("terminal-output").display(),
        brain.join("SOUL.md").display(),
        brain.join("INDEX.md").display(),
        brain.join("MEMORY.md").display(),
        brain.join("USER.md").display(),
        brain.join("skills").display(),
        socket.display(),
    );
    let path = dir.join("ulei.toml");
    fs::write(&path, text).unwrap();
    path
}

/// `ulei-engine <args>` in `dir` with an environment of `PATH` and `HOME` only.
fn command(dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(BIN);
    command
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir);
    command
}

/// Starts `serve` with the settings at `config` and waits for its ready line.
fn start(dir: &Path, config: &Path) -> Engine {
    let mut child = command(dir, &["serve", "--config", config.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let mut stderr = String::new();
    if line.is_empty() {
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
    }
    let socket = line
        .strip_prefix("ulei-engine ready ")
        .unwrap_or_else(|| panic!("no ready line: {stderr}"))
        .trim_end()
        .into();
    Engine { child, socket }
}

/// A gateway with `script` and a started engine in a fresh directory.
fn setup(script: Vec<Answer>) -> (FakeGateway, Scratch, Engine) {
    let fake = FakeGateway::start(script);
    let dir = Scratch::new();
    let config = config(&dir.0, fake.socket(), &dir.path("c6.sock"));
    let engine = start(&dir.0, &config);
    (fake, dir, engine)
}

/// One answer as read from the wire.
struct Response {
    status: u16,
    head: String,
    body: Vec<u8>,
}

/// Sends `raw` on a fresh connection and reads the answer up to the end of the stream.
fn exchange(socket: &Path, raw: &[u8]) -> Response {
    let mut stream = connect(socket, raw);
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer).unwrap();
    parse(&answer)
}

fn connect(socket: &Path, raw: &[u8]) -> UnixStream {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream.write_all(raw).unwrap();
    stream
}

/// The answer to `raw`, which the server refuses before reading its last byte: closing with
/// unread bytes resets the stream after the answer.
fn exchange_refused(socket: &Path, raw: &[u8]) -> Response {
    let mut stream = connect(socket, raw);
    let mut answer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return parse(&answer),
            Err(_) if !answer.is_empty() => return parse(&answer),
            Ok(n) => answer.extend_from_slice(&chunk[..n]),
            Err(e) => panic!("read: {e}"),
        }
    }
}

fn parse(answer: &[u8]) -> Response {
    let end = answer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("no head in {:?}", String::from_utf8_lossy(answer)));
    let head = String::from_utf8(answer[..end].to_vec()).unwrap();
    let status = head[9..12].parse().unwrap();
    let body = answer[end + 4..].to_vec();
    let length = format!("\r\nContent-Length: {}\r\n", body.len());
    assert!(head.starts_with("HTTP/1.1 "), "{head}");
    assert!(
        head.contains("\r\nContent-Type: application/json\r\n"),
        "{head}"
    );
    assert!(
        head.contains(&length),
        "exactly the body, then the end: {head}"
    );
    assert!(head.ends_with("\r\nConnection: close"), "{head}");
    Response { status, head, body }
}

fn request(method: &str, body: &[u8]) -> Vec<u8> {
    let mut raw = format!(
        "POST /v1/{method} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    raw.extend_from_slice(body);
    raw
}

fn post(socket: &Path, method: &str, body: &Value) -> Response {
    exchange(socket, &request(method, body.to_string().as_bytes()))
}

fn ok(response: &Response) -> &[u8] {
    assert_eq!(
        response.status,
        200,
        "{}",
        String::from_utf8_lossy(&response.body)
    );
    &response.body
}

fn submit(socket: &Path, body: &Value) -> SubmitTurnResponse {
    serde_json::from_slice(ok(&post(socket, "submit_turn", body))).unwrap()
}

fn prompt(socket: &Path, session: &str, prompt: &str) -> String {
    let answer = submit(socket, &json!({"session": session, "prompt": prompt}));
    assert!(answer.accepted);
    answer.turn.unwrap()
}

fn status(socket: &Path, session: &str, turn: Option<&str>) -> SessionStatus {
    let mut body = json!({"session": session});
    if let Some(turn) = turn {
        body["turn"] = turn.into();
    }
    serde_json::from_slice(ok(&post(socket, "status", &body))).unwrap()
}

fn report(socket: &Path, session: &str, turn: &str) -> TurnReport {
    status(socket, session, Some(turn)).turn.unwrap()
}

fn is_idle(socket: &Path) -> IdleStatus {
    serde_json::from_slice(ok(&post(socket, "is_idle", &json!({})))).unwrap()
}

/// The report of `turn` once it has ended, waiting at most 10 s.
fn ended(socket: &Path, session: &str, turn: &str) -> TurnReport {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let report = report(socket, session, turn);
        if report.state == TurnState::Ended {
            return report;
        }
        assert!(Instant::now() < deadline, "{turn} did not end: {report:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A turn the fake answers with `reply` once released.
fn held(reply: &str) -> Answer {
    Answer::Held(vec![route(), text(reply), usage(3), stop(StopReason::End)])
}

/// The text of the last user message of a recorded turn request.
fn last_prompt(body: &[u8]) -> String {
    let value: Value = serde_json::from_slice(body).unwrap();
    let user = value["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .unwrap()
        .clone();
    user["content"][0]["text"].as_str().unwrap().to_owned()
}

/// `(turn, user text)` of each user message of session `session` in the journal.
fn journaled_prompts(dir: &Scratch, session: u64) -> Vec<(u64, String)> {
    let journal = Journal::open(&dir.path("journal.sqlite3")).unwrap();
    journal
        .records(session)
        .unwrap()
        .into_iter()
        .filter(|m| m.message["role"] == "user")
        .map(|m| {
            (
                m.turn,
                m.message["content"][0]["text"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

#[test]
fn each_served_method_answers_200_with_its_output() {
    let (_fake, _dir, engine) = setup(vec![text_step("hello")]);
    let s = &engine.socket;

    let idle = is_idle(s);
    assert!(idle.idle);
    assert!(idle.since.unwrap() <= now());

    let empty = status(s, "fresh", None);
    assert_eq!(
        (empty.running, empty.queue_depth, empty.route.as_str()),
        (false, 0, "main")
    );
    assert_eq!((empty.last_turn, empty.turn), (None, None));

    let answer = submit(
        s,
        &json!({"session": "chat", "prompt": "hi", "channel": "cli"}),
    );
    assert!(answer.accepted);
    assert_eq!((answer.position, answer.queue_depth), (Some(0), 0));
    let turn = answer.turn.unwrap();
    let report = ended(s, "chat", &turn);
    assert_eq!(report.outcome, Some(TurnOutcome::Completed));
    assert_eq!(report.line, None);
    let after = status(s, "chat", None);
    assert_eq!(after.last_turn, Some(report.clone()));
    assert_eq!(after.since, report.ended.unwrap());
    assert!(!after.running);
}

#[test]
fn each_error_table_row_is_answered_with_its_c6_error() {
    let (_fake, _dir, engine) = setup(Vec::new());
    let s = &engine.socket;
    let code = |raw: &[u8]| {
        let response = exchange(s, raw);
        let error: WireError = serde_json::from_slice(&response.body).unwrap();
        (response.status, error.code)
    };
    let bad = (400, ErrorCode::BadRequest);
    let not_found = (404, ErrorCode::NotFound);
    let too_large = (413, ErrorCode::TooLarge);

    assert_eq!(code(b"nonsense\r\n\r\n"), bad);
    assert_eq!(
        code(b"POST /v1/status HTTP/1.0\r\nContent-Length: 2\r\n\r\n{}"),
        bad
    );
    assert_eq!(code(b"POST /v1/status HTTP/1.1\r\n\r\n"), bad);
    assert_eq!(
        code(
            b"POST /v1/is_idle HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n"
        ),
        bad
    );
    assert_eq!(
        code(b"POST /v1/is_idle HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}{}"),
        bad,
        "bytes after the body"
    );
    for (method, body) in [
        ("is_idle", "not json"),
        ("is_idle", "[]"),
        ("is_idle", r#"{"extra":1}"#),
        ("status", r#"{"session":""}"#),
        ("status", &format!(r#"{{"session":"{}"}}"#, "k".repeat(129))),
        ("status", r#"{"session":"chat","turn":"no spaces"}"#),
        ("status", r#"{"session":"chat","turn":null}"#),
        ("submit_turn", r#"{"session":"chat","prompt":""}"#),
        (
            "submit_turn",
            r#"{"session":"chat","prompt":"hi","channel":""}"#,
        ),
        (
            "submit_turn",
            r#"{"session":"chat","prompt":"hi","new":"yes"}"#,
        ),
        (
            "submit_turn",
            r#"{"session":"chat","prompt":"hi","other":1}"#,
        ),
        ("submit_turn", r#"["chat","hi"]"#),
    ] {
        assert_eq!(
            code(&request(method, body.as_bytes())),
            bad,
            "{method} {body}"
        );
    }

    assert_eq!(
        code(b"GET /v1/status HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}"),
        not_found
    );
    assert_eq!(code(&request("unknown", b"{}")), not_found);
    assert_eq!(
        code(b"POST /status HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}"),
        not_found
    );
    let inject =
        json!({"event_id": "e1", "kind": "weekly", "payload": "p", "lease": "l", "attempt": 1});
    for (method, body) in [
        ("inject", inject),
        ("interrupt", json!({"session": "chat"})),
        ("set_route", json!({"session": "chat", "route": "main"})),
    ] {
        assert_eq!(
            code(&request(method, body.to_string().as_bytes())),
            not_found
        );
    }

    let mut long = b"POST /v1/is_idle HTTP/1.1\r\nX-Pad: ".to_vec();
    long.resize(8 * 1024 + 16, b'a');
    long.extend_from_slice(b"\r\nContent-Length: 2\r\n\r\n{}");
    let refused = exchange_refused(s, &long);
    let error: WireError = serde_json::from_slice(&refused.body).unwrap();
    assert_eq!((refused.status, error.code), too_large);
    assert_eq!(
        code(b"POST /v1/is_idle HTTP/1.1\r\nContent-Length: 524289\r\n\r\n"),
        too_large
    );
    let mut padded = b"{}".to_vec();
    padded.resize(512 * 1024, b' ');
    assert_eq!(exchange(s, &request("is_idle", &padded)).status, 200);
}

#[test]
fn prompts_submitted_during_a_turn_queue_and_run_one_by_one_in_order() {
    let (fake, dir, engine) = setup(vec![held("one"), text_step("two"), text_step("three")]);
    let s = &engine.socket;
    let first = prompt(s, "chat", "first");
    assert!(fake.wait_hanging("s1-t1-"));

    let second = submit(s, &json!({"session": "chat", "prompt": "second"}));
    let third = submit(s, &json!({"session": "chat", "prompt": "third"}));
    assert_eq!((second.position, second.queue_depth), (Some(1), 1));
    assert_eq!((third.position, third.queue_depth), (Some(2), 2));
    let (second, third) = (second.turn.unwrap(), third.turn.unwrap());
    let busy = status(s, "chat", Some(&second));
    assert_eq!((busy.running, busy.queue_depth), (true, 2));
    assert_eq!(busy.turn.unwrap().state, TurnState::Queued);
    assert_eq!(report(s, "chat", &first).state, TurnState::Running);

    fake.release();
    let last = ended(s, "chat", &third);
    for turn in [&first, &second] {
        let report = ended(s, "chat", turn);
        assert_eq!(report.outcome, Some(TurnOutcome::Completed));
        assert!(report.ended.unwrap() <= last.ended.unwrap());
    }
    let prompts: Vec<String> = turns(&fake).iter().map(|b| last_prompt(b)).collect();
    assert_eq!(prompts, ["first", "second", "third"]);
    assert_eq!(
        journaled_prompts(&dir, 1),
        [
            (1, "first".to_owned()),
            (2, "second".to_owned()),
            (3, "third".to_owned())
        ]
    );
    let done = status(s, "chat", None);
    assert_eq!((done.running, done.queue_depth), (false, 0));
    assert_eq!(done.last_turn.unwrap().turn, third);
}

#[test]
fn a_prompt_over_the_32_queued_turns_is_refused() {
    let (fake, _dir, engine) = setup(vec![held("one")]);
    let s = &engine.socket;
    prompt(s, "chat", "running");
    assert!(fake.wait_hanging("s1-t1-"));
    for n in 1..=MAX_QUEUE as u64 {
        let answer = submit(
            s,
            &json!({"session": "chat", "prompt": format!("queued {n}")}),
        );
        assert!(answer.accepted);
        assert_eq!((answer.position, answer.queue_depth), (Some(n), n));
    }
    let refused = submit(s, &json!({"session": "chat", "prompt": "one too many"}));
    assert_eq!(
        refused,
        SubmitTurnResponse {
            accepted: false,
            turn: None,
            position: None,
            queue_depth: 32,
        }
    );
    assert_eq!(status(s, "chat", None).queue_depth, 32);
}

#[test]
fn new_starts_a_new_session_of_the_key_when_its_turn_starts() {
    let (fake, dir, engine) = setup(vec![held("one"), text_step("two"), text_step("three")]);
    let s = &engine.socket;
    prompt(s, "chat", "first");
    assert!(fake.wait_hanging("s1-t1-"));
    prompt(s, "chat", "second");
    let fresh = submit(
        s,
        &json!({"session": "chat", "prompt": "third", "new": true}),
    );
    fake.release();
    ended(s, "chat", &fresh.turn.unwrap());

    assert_eq!(
        journaled_prompts(&dir, 1),
        [(1, "first".to_owned()), (2, "second".to_owned())]
    );
    assert_eq!(journaled_prompts(&dir, 2), [(1, "third".to_owned())]);
    let third: Value = serde_json::from_slice(&turns(&fake)[2]).unwrap();
    assert_eq!(third["messages"].as_array().unwrap().len(), 1);
}

#[test]
fn turns_of_two_sessions_run_at_the_same_time_one_per_session() {
    let (fake, dir, engine) = setup(vec![held("left"), text_step("right")]);
    let s = &engine.socket;
    let left = prompt(s, "left", "to the left");
    assert!(fake.wait_hanging("s1-t1-"));
    let right = submit(s, &json!({"session": "right", "prompt": "to the right"}));
    assert_eq!(right.position, Some(0));
    let right = ended(s, "right", &right.turn.unwrap());
    assert_eq!(right.outcome, Some(TurnOutcome::Completed));
    assert_eq!(report(s, "left", &left).state, TurnState::Running);

    fake.release();
    ended(s, "left", &left);
    assert_eq!(journaled_prompts(&dir, 1), [(1, "to the left".to_owned())]);
    assert_eq!(journaled_prompts(&dir, 2), [(1, "to the right".to_owned())]);
}

#[test]
fn turn_ids_never_repeat_across_a_restart_and_last_turn_comes_from_the_journal() {
    let fake = FakeGateway::start(vec![text_step("one"), text_step("two")]);
    let dir = Scratch::new();
    let config = config(&dir.0, fake.socket(), &dir.path("c6.sock"));

    let engine = start(&dir.0, &config);
    let before = prompt(&engine.socket, "chat", "first");
    ended(&engine.socket, "chat", &before);
    drop(engine);

    let engine = start(&dir.0, &config);
    let s = &engine.socket;
    let after_restart = status(s, "chat", Some(&before));
    assert_eq!(after_restart.turn.unwrap().state, TurnState::Unknown);
    assert_eq!(
        after_restart.last_turn,
        Some(TurnReport {
            turn: "s1-t1".to_owned(),
            state: TurnState::Ended,
            outcome: None,
            ended: None,
            line: None,
        })
    );
    assert!(!after_restart.running);

    let after = prompt(s, "chat", "second");
    assert_ne!(after, before);
    let last = ended(s, "chat", &after);
    assert_eq!(status(s, "chat", None).last_turn, Some(last));
    assert_eq!(report(s, "chat", &before).state, TurnState::Unknown);
}

#[test]
fn a_quota_exhausted_turn_reports_the_line_run_prints() {
    let mut quota = gateway_error(ErrorClass::QuotaExhausted);
    quota.resets_at = Some(1_700_000_000);
    let fake = FakeGateway::start(vec![
        Answer::Reject(429, quota.clone()),
        Answer::Reject(429, quota),
    ]);
    let dir = Scratch::new();
    let config = config(&dir.0, fake.socket(), &dir.path("c6.sock"));
    let serving = start(&dir.0, &config);
    let turn = prompt(&serving.socket, "chat", "hi");
    let report = ended(&serving.socket, "chat", &turn);
    assert_eq!(report.outcome, Some(TurnOutcome::QuotaExhausted));
    drop(serving);

    let run = command(
        &dir.0,
        &[
            "run",
            "--session",
            "chat",
            "--route",
            "main",
            "--prompt",
            "hi",
            "--config",
            config.to_str().unwrap(),
        ],
    )
    .output()
    .unwrap();
    assert_eq!(run.status.code(), Some(3));
    assert_eq!(
        format!("{}\n", report.line.unwrap()).into_bytes(),
        run.stderr
    );
}

#[test]
fn is_idle_reports_the_owner_session() {
    let (fake, _dir, engine) = setup(vec![held("busy"), text_step("other")]);
    let s = &engine.socket;
    let started = is_idle(s).since.unwrap();

    let turn = prompt(s, OWNER, "work");
    assert!(fake.wait_hanging("s1-t1-"));
    assert_eq!(
        is_idle(s),
        IdleStatus {
            idle: false,
            since: None
        }
    );
    let queued = prompt(s, OWNER, "more");
    fake.release();
    ended(s, OWNER, &turn);
    let last = ended(s, OWNER, &queued);
    let idle = is_idle(s);
    assert!(idle.idle);
    assert_eq!(idle.since, last.ended);
    assert!(idle.since.unwrap() >= started);
}

#[test]
fn the_socket_is_0600_and_a_directory_open_to_others_is_refused() {
    let (_fake, _dir, engine) = setup(Vec::new());
    let mode = fs::metadata(&engine.socket).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);

    let fake = FakeGateway::start(Vec::new());
    let dir = Scratch::new();
    let open = dir.path("open");
    fs::DirBuilder::new().mode(0o711).create(&open).unwrap();
    fs::set_permissions(&open, fs::Permissions::from_mode(0o711)).unwrap();
    let socket = open.join("c6.sock");
    let config = config(&dir.0, fake.socket(), &socket);
    let out = command(&dir.0, &["serve", "--config", config.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(out.stderr).unwrap(),
        format!(
            "ulei-engine: {}: socket directory {} must belong to this user with mode 0700 or stricter\n",
            socket.display(),
            open.display()
        )
    );
    assert!(!socket.exists());
}

#[test]
fn a_missing_or_invalid_serve_setting_names_its_key_and_variable() {
    let fake = FakeGateway::start(Vec::new());
    let dir = Scratch::new();
    let config = config(&dir.0, fake.socket(), &dir.path("c6.sock"));
    let base = fs::read_to_string(&config).unwrap();
    for (from, to, line) in [
        (
            "owner_session = \"owner\"\n",
            "",
            "ulei-engine: missing engine.owner_session: set it in the config file or in ULEI_ENGINE_OWNER_SESSION\n",
        ),
        (
            "default_route = \"main\"\n",
            "default_route = \"Main\"\n",
            "ulei-engine: engine.default_route (ULEI_ENGINE_DEFAULT_ROUTE) must be a qb route alias\n",
        ),
    ] {
        fs::write(&config, base.replace(from, to)).unwrap();
        let out = command(&dir.0, &["serve", "--config", config.to_str().unwrap()])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1));
        assert_eq!(String::from_utf8(out.stderr).unwrap(), line);
    }
    let out = command(&dir.0, &["serve", "--config", config.to_str().unwrap()])
        .env("ULEI_ENGINE_DEFAULT_ROUTE", "main")
        .env("ULEI_ENGINE_C6_SOCKET", "")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(out.stderr).unwrap(),
        "ulei-engine: engine.c6_socket (ULEI_ENGINE_C6_SOCKET) is empty\n"
    );
}

/// Answers every turn at once as completed.
struct Instantly;

impl Runner for Instantly {
    fn run(&self, _: &SubmitTurnRequest) -> Ending {
        Ending {
            outcome: TurnOutcome::Completed,
            line: None,
        }
    }
}

/// A server over a journal in `dir` that serves peers of `uid` with `timeout`.
fn server(dir: &Scratch, uid: u32, timeout: Duration) -> Server {
    let journal = Journal::open(&dir.path("journal.sqlite3")).unwrap();
    Server::new(
        journal,
        Arc::new(Instantly),
        (OWNER.to_owned(), "main".to_owned()),
        uid,
        timeout,
    )
}

/// Serves `sent` on one end of a socketpair; the raw answer and how long serving took.
fn serve_pair(server: &Server, sent: &[u8]) -> (Vec<u8>, Duration) {
    let (mut client, end) = UnixStream::pair().unwrap();
    client.write_all(sent).unwrap();
    let started = Instant::now();
    server.serve(&end);
    let elapsed = started.elapsed();
    drop(end);
    let mut answer = Vec::new();
    client.read_to_end(&mut answer).unwrap();
    (answer, elapsed)
}

fn own_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

#[test]
fn a_peer_of_another_uid_gets_403_before_its_request_is_read() {
    let dir = Scratch::new();
    let server = server(&dir, own_uid().wrapping_add(1), READ_TIMEOUT);
    // Nothing is sent: a server that read first would wait out the deadline instead.
    let (raw, elapsed) = serve_pair(&server, b"");
    let response = parse(&raw);
    assert_eq!(response.status, 403, "{}", response.head);
    let error: WireError = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(error.code, ErrorCode::Forbidden);
    assert!(elapsed < READ_TIMEOUT, "{elapsed:?}");
}

#[test]
fn a_request_not_whole_within_the_deadline_is_closed_unanswered() {
    let dir = Scratch::new();
    let deadline = Duration::from_millis(300);
    let server = server(&dir, own_uid(), deadline);
    let (raw, elapsed) = serve_pair(&server, b"POST /v1/is_idle HTTP/1.1\r\nContent-Le");
    assert_eq!(raw, b"");
    assert!(elapsed >= deadline && elapsed < READ_TIMEOUT, "{elapsed:?}");
    let (raw, _) = serve_pair(
        &server,
        b"POST /v1/is_idle HTTP/1.1\r\nContent-Length: 2\r\n\r\n{",
    );
    assert_eq!(raw, b"", "a body still short at the deadline");
}

#[test]
fn the_reports_of_the_last_64_ended_turns_of_a_session_are_kept() {
    let dir = Scratch::new();
    let server = server(&dir, own_uid(), READ_TIMEOUT);
    let mut call = |method: &str, body: Value| {
        let (raw, _) = serve_pair(&server, &request(method, body.to_string().as_bytes()));
        parse(&raw).body
    };
    let mut ids = Vec::new();
    for n in 0..=KEPT_REPORTS {
        let answer: SubmitTurnResponse = serde_json::from_slice(&call(
            "submit_turn",
            json!({"session": "chat", "prompt": format!("turn {n}")}),
        ))
        .unwrap();
        let id = answer.turn.unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let status: SessionStatus =
                serde_json::from_slice(&call("status", json!({"session": "chat", "turn": id})))
                    .unwrap();
            if status.turn.unwrap().state == TurnState::Ended {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        ids.push(id);
    }
    let state = |call: &mut dyn FnMut(&str, Value) -> Vec<u8>, id: &str| {
        let status: SessionStatus =
            serde_json::from_slice(&call("status", json!({"session": "chat", "turn": id})))
                .unwrap();
        status.turn.unwrap().state
    };
    assert_eq!(state(&mut call, &ids[0]), TurnState::Unknown);
    for id in &ids[1..] {
        assert_eq!(state(&mut call, id), TurnState::Ended, "{id}");
    }
    let unique: std::collections::HashSet<&String> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len());
}

#[test]
fn a_journal_that_cannot_be_read_answers_500_internal() {
    let dir = Scratch::new();
    let server = server(&dir, own_uid(), READ_TIMEOUT);
    rusqlite::Connection::open(dir.path("journal.sqlite3"))
        .unwrap()
        .execute_batch("DROP TABLE steps")
        .unwrap();
    let body = json!({"session": "chat"}).to_string();
    let (raw, _) = serve_pair(&server, &request("status", body.as_bytes()));
    let response = parse(&raw);
    assert_eq!(response.status, 500, "{}", response.head);
    let error: WireError = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(error.code, ErrorCode::Internal);
}
