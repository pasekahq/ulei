//! `ulei-engine serve` through the built binary and raw bytes on its C6 socket, its turns on
//! the fake gateway; the peer check, the request deadline, the kept reports and a late
//! interrupt through `ulei_engine::serve::Server` on a socketpair.

#![allow(missing_docs, clippy::unwrap_used)]

mod c6_wire;
mod fake_gateway;
mod fake_mcp;

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use c6_wire::{
    OWNER, command, config, connect, ended, events_request, exchange, exchange_refused, hold_inbox,
    inject, interrupt, is_idle, open_inbox, parse, prompt, read_inbox, read_page, report,
    report_delivery, request, set_route, setup, start, status, submit, turn_events, until_ended,
};
use fake_gateway::{
    Answer, FakeGateway, SUMMARY_REQUEST, Scratch, call, cancels, dead, error_event, gateway_error,
    pid_file, route, routes, stop, text, text_step, thinking, tool_step, turns, untagged, usage,
};
use fake_mcp::{FakeMcp, Spec};
use serde_json::{Value, json};
use ulei_contracts::generated::c4g::{ErrorClass, StopReason};
use ulei_contracts::generated::c6::{
    Ack, Delivery, Error as WireError, ErrorCode, IdleStatus, InboxTurns, InjectResponse,
    InterruptResponse, RouteSet, SessionStatus, SubmitTurnResponse, TurnEventType, TurnEventsPage,
    TurnOutcome, TurnReport, TurnState,
};
use ulei_contracts::generated::c8::{InboxDelivery, InboxDeliveryDelivery};
use ulei_engine::gateway::{CANCEL_TIMEOUT, MAX_BODY};
use ulei_engine::journal::Journal;
use ulei_engine::serve::{
    Ending, Input, KEPT_REPORTS, MAX_QUEUE, READ_TIMEOUT, RETRY_AFTER_S, Runner, Server,
};
use ulei_engine::tools::Interrupt;

/// Waits at most 10 s until the fake has recorded a cancel request.
fn cancel_arrived(fake: &FakeGateway) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while cancels(fake).is_empty() {
        assert!(Instant::now() < deadline, "no cancel request");
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

/// A step that says `said` and calls `search_files` as call `id`, stopping for tool use.
fn narrated(said: &str, id: &str) -> Answer {
    Answer::Events(vec![
        route(),
        text(said),
        call(
            id,
            "search_files",
            json!({"pattern": "*", "target": "files"}),
        ),
        stop(StopReason::ToolUse),
    ])
}

/// The texts of the user messages of a recorded turn request that carry text, in order.
fn user_texts(body: &[u8]) -> Vec<String> {
    let value: Value = serde_json::from_slice(body).unwrap();
    value["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user" && m["content"][0]["type"] == "text")
        .map(|m| m["content"][0]["text"].as_str().unwrap().to_owned())
        .collect()
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

/// The events of `page` as JSON.
fn events(page: &TurnEventsPage) -> Value {
    serde_json::to_value(&page.events).unwrap()
}

/// Asserts that no answer arrives on `stream` within 200 ms.
fn waiting(mut stream: &UnixStream) {
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let read = stream.read(&mut [0u8; 1]);
    assert!(
        matches!(&read, Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)),
        "{read:?}"
    );
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
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

    let _hold = open_inbox(s);
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
        (
            "submit_turn",
            r#"{"session":"chat","prompt":"hi","context":""}"#,
        ),
        ("submit_turn", r#"["chat","hi"]"#),
        ("interrupt", r#"{"session":""}"#),
        ("interrupt", r#"{"session":"chat","turn":"x"}"#),
        ("turn_events", r#"{"session":"chat","turn":"t-1"}"#),
        (
            "turn_events",
            r#"{"session":"chat","turn":"t 1","after":0}"#,
        ),
        ("turn_events", r#"{"session":"","turn":"t-1","after":0}"#),
        (
            "turn_events",
            r#"{"session":"chat","turn":"t-1","after":-1}"#,
        ),
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
fn a_set_route_or_inject_body_off_its_schema_answers_400() {
    let (fake, _dir, engine) = setup(Vec::new());
    for (method, body) in [
        ("set_route", r#"{"session":"chat","route":"Deep"}"#),
        ("set_route", r#"{"session":"chat","route":"deep/model"}"#),
        ("set_route", r#"{"session":"chat","route":""}"#),
        ("set_route", r#"{"session":"","route":"deep"}"#),
        ("set_route", r#"{"session":"chat"}"#),
        (
            "inject",
            r#"{"event_id":"e 1","kind":"k","prompt":"p","lease":"l","attempt":1}"#,
        ),
        (
            "inject",
            r#"{"event_id":"e1","kind":"k","prompt":"","lease":"l","attempt":1}"#,
        ),
        (
            "inject",
            r#"{"event_id":"e1","kind":"k","prompt":"p","lease":"l","attempt":0}"#,
        ),
        (
            "inject",
            r#"{"event_id":"e1","kind":"k","prompt":"p","attempt":1}"#,
        ),
    ] {
        let response = exchange(&engine.socket, &request(method, body.as_bytes()));
        let error: WireError = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(
            (response.status, error.code),
            (400, ErrorCode::BadRequest),
            "{method} {body}"
        );
    }
    assert_eq!(status(&engine.socket, "chat", None).route, "main");
    assert_eq!(fake.requests().len(), 0);
}

#[test]
fn a_client_writing_a_body_over_512_kib_reads_the_413_then_a_clean_end() {
    let (_fake, _dir, engine) = setup(Vec::new());
    let mut body = b"{}".to_vec();
    body.resize(600 * 1024, b' ');
    let mut stream = connect(&engine.socket, &request("is_idle", &body));
    let mut answer = Vec::new();
    stream
        .read_to_end(&mut answer)
        .expect("the answer ends without a reset");
    let response = parse(&answer);
    let error: WireError = serde_json::from_slice(&response.body).unwrap();
    assert_eq!((response.status, error.code), (413, ErrorCode::TooLarge));
}

#[test]
fn an_interrupt_answers_before_the_turn_unwinds_and_a_later_turn_runs_in_the_same_history() {
    let (fake, dir, engine) = setup(vec![held("never"), text_step("after")]);
    let s = &engine.socket;
    let first = prompt(s, "chat", "first");
    assert!(fake.wait_hanging("s1-t1-"));
    fake.hold_cancels();

    let answer = interrupt(s, "chat");
    assert!(answer.interrupted);
    assert_eq!(answer.turn.as_deref(), Some(first.as_str()));
    assert_eq!((answer.dropped, answer.queue_depth), (None, 0));
    // The turn waits for the answer to its cancel, so it cannot have ended yet.
    cancel_arrived(&fake);
    let unwinding = status(s, "chat", Some(&first));
    assert!(unwinding.running);
    assert_eq!(unwinding.turn.unwrap().state, TurnState::Running);
    assert_eq!(unwinding.last_turn, None);
    let again = interrupt(s, "chat");
    assert!(!again.interrupted);
    assert_eq!(
        (again.turn, again.dropped, again.queue_depth),
        (None, None, 0)
    );

    fake.answer_cancels();
    let report = ended(s, "chat", &first);
    assert_eq!(report.outcome, Some(TurnOutcome::Interrupted));
    assert_eq!(report.line, None);
    let idle = status(s, "chat", None);
    assert!(!idle.running);
    assert_eq!(idle.last_turn, Some(report));
    let cancelled = cancels(&fake);
    assert_eq!(
        cancelled.iter().map(|id| untagged(id)).collect::<Vec<_>>(),
        ["s1-t1-0"]
    );

    let noop = interrupt(s, "chat");
    assert!(!noop.interrupted);
    assert_eq!((noop.turn, noop.dropped, noop.queue_depth), (None, None, 0));
    assert_eq!(status(s, "chat", None), idle);
    let unknown = interrupt(s, "nobody");
    assert!(!unknown.interrupted);
    assert_eq!(status(s, "nobody", None).last_turn, None);

    let second = prompt(s, "chat", "second");
    assert_eq!(
        ended(s, "chat", &second).outcome,
        Some(TurnOutcome::Completed)
    );
    let sent = turns(&fake);
    let later: Value = serde_json::from_slice(&sent[1]).unwrap();
    assert!(
        later["request_id"].as_str().unwrap().starts_with("s1-t2-"),
        "{later}"
    );
    let texts: Vec<&str> = later["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| m["content"][0]["text"].as_str().unwrap())
        .collect();
    assert_eq!(texts, ["first", "second"]);
    assert_eq!(
        journaled_prompts(&dir, 1),
        [(1, "first".to_owned()), (2, "second".to_owned())]
    );
}

#[test]
fn an_interrupt_kills_the_process_group_of_a_running_shell_tool() {
    let command = "(sleep 30 & echo $! > child.pid; wait) & echo $$ > shell.pid; wait";
    let (fake, dir, engine) = setup(vec![tool_step(vec![call(
        "c1",
        "terminal",
        json!({"command": command}),
    )])]);
    let s = &engine.socket;
    let turn = prompt(s, "chat", "run it");
    let (child, shell) = (
        pid_file(&dir.path("child.pid")),
        pid_file(&dir.path("shell.pid")),
    );
    assert!(!dead(&child));

    assert!(interrupt(s, "chat").interrupted);
    assert_eq!(
        ended(s, "chat", &turn).outcome,
        Some(TurnOutcome::Interrupted)
    );
    assert!(dead(&child), "the backgrounded child {child}");
    assert!(dead(&shell), "the shell {shell}");
    assert_eq!(turns(&fake).len(), 1, "no step after the tool call");
}

#[test]
fn an_interrupt_during_the_first_mcp_connect_ends_the_turn_at_once() {
    let mcp = FakeMcp::start(Spec::new(Vec::new()));
    mcp.set_down(usize::MAX);
    let fake = FakeGateway::start(vec![text_step("unreachable")]);
    let dir = Scratch::new();
    let path = config(&dir.0, fake.socket(), &dir.path("c6.sock"));
    let servers = format!(
        "mcp_servers = \"notes\"\n[engine.mcp.notes]\nurl = \"{}\"\n",
        mcp.url()
    );
    fs::write(&path, fs::read_to_string(&path).unwrap() + &servers).unwrap();
    let engine = start(&dir.0, &path);
    let s = &engine.socket;
    let turn = prompt(s, "chat", "first");
    assert!(mcp.wait_for("initialize"));

    let started = Instant::now();
    assert!(interrupt(s, "chat").interrupted);
    let report = ended(s, "chat", &turn);
    assert_eq!(report.outcome, Some(TurnOutcome::Interrupted));
    // The three backoffs of the first connect wait at least 5.6 s together.
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(turns(&fake).len(), 0);
    assert_eq!(journaled_prompts(&dir, 1), [(1, "first".to_owned())]);
}

#[test]
fn an_interrupt_drops_the_queue_head_and_the_rest_run_in_order_after_the_turn() {
    let (fake, dir, engine) = setup(vec![held("never"), text_step("3"), text_step("4")]);
    let s = &engine.socket;
    let first = prompt(s, "chat", "first");
    assert!(fake.wait_hanging("s1-t1-"));
    let queued: Vec<String> = ["second", "third", "fourth"]
        .iter()
        .map(|p| prompt(s, "chat", p))
        .collect();
    fake.hold_cancels();

    let answer = interrupt(s, "chat");
    assert!(answer.interrupted);
    assert_eq!(answer.turn.as_deref(), Some(first.as_str()));
    assert_eq!(answer.dropped.as_deref(), Some(queued[0].as_str()));
    assert_eq!(answer.queue_depth, 2);
    cancel_arrived(&fake);
    let dropped = report(s, "chat", &queued[0]);
    assert_eq!(dropped.state, TurnState::Dropped);
    assert!(dropped.ended.unwrap() <= now());
    assert_eq!(dropped.outcome, None);
    let waiting = status(s, "chat", Some(&queued[1]));
    assert_eq!((waiting.running, waiting.queue_depth), (true, 2));
    assert_eq!(waiting.turn.unwrap().state, TurnState::Queued);
    assert_eq!(turns(&fake).len(), 1, "nothing starts before the turn ends");

    fake.answer_cancels();
    let last = ended(s, "chat", &queued[2]);
    let third = ended(s, "chat", &queued[1]);
    assert_eq!(
        ended(s, "chat", &first).outcome,
        Some(TurnOutcome::Interrupted)
    );
    assert_eq!(third.outcome, Some(TurnOutcome::Completed));
    assert_eq!(last.outcome, Some(TurnOutcome::Completed));
    assert!(third.ended.unwrap() <= last.ended.unwrap());
    let prompts: Vec<String> = turns(&fake).iter().map(|b| last_prompt(b)).collect();
    assert_eq!(prompts, ["first", "third", "fourth"]);
    assert_eq!(report(s, "chat", &queued[0]).state, TurnState::Dropped);
    assert_eq!(
        journaled_prompts(&dir, 1),
        [
            (1, "first".to_owned()),
            (2, "third".to_owned()),
            (3, "fourth".to_owned())
        ]
    );
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
    let kept = turn_events(&engine.socket, "chat", &before, 0);
    assert_eq!(kept.events.len(), 3);
    drop(engine);

    let engine = start(&dir.0, &config);
    let s = &engine.socket;
    let after_restart = status(s, "chat", Some(&before));
    assert_eq!(after_restart.turn.unwrap().state, TurnState::Unknown);
    let unknown = turn_events(s, "chat", &before, 0);
    assert_eq!(
        (unknown.state, unknown.events),
        (TurnState::Unknown, Vec::new())
    );
    assert_eq!(
        after_restart.last_turn,
        Some(TurnReport {
            turn: "s1-t1".to_owned(),
            state: TurnState::Ended,
            outcome: None,
            ended: None,
            line: None,
            delivery: None,
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
    let _hold = open_inbox(s);
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
    fn run(&self, _: &Input, _: &Interrupt, _: &mut dyn FnMut(&str)) -> Ending {
        Ending {
            outcome: TurnOutcome::Completed,
            line: None,
            answer: None,
        }
    }
}

/// A server over a journal in `dir` that serves peers of `uid` with `timeout`.
fn server(dir: &Scratch, uid: u32, timeout: Duration) -> Server {
    server_with(dir, Arc::new(Instantly), uid, timeout)
}

/// A server whose turns `runner` runs, over a journal in `dir`.
fn server_with(dir: &Scratch, runner: Arc<dyn Runner>, uid: u32, timeout: Duration) -> Server {
    let journal = Journal::open(&dir.path("journal.sqlite3")).unwrap();
    Server::new(
        journal,
        runner,
        (OWNER.to_owned(), "main".to_owned()),
        uid,
        timeout,
    )
}

/// Serves `sent` on one end of a socketpair while the other end reads; the raw answer and how
/// long serving took.
fn serve_pair(server: &Server, sent: &[u8]) -> (Vec<u8>, Duration) {
    let (mut client, end) = UnixStream::pair().unwrap();
    client.write_all(sent).unwrap();
    std::thread::scope(|scope| {
        let reader = scope.spawn(move || {
            let mut answer = Vec::new();
            client.read_to_end(&mut answer).unwrap();
            answer
        });
        let started = Instant::now();
        server.serve(&end);
        let elapsed = started.elapsed();
        drop(end);
        (reader.join().unwrap(), elapsed)
    })
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
        until_ended(&id, || {
            let status: SessionStatus =
                serde_json::from_slice(&call("status", json!({"session": "chat", "turn": id})))
                    .unwrap();
            status.turn.unwrap()
        });
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
fn a_journal_locked_past_the_request_deadline_answers_500_within_it() {
    let dir = Scratch::new();
    let deadline = Duration::from_millis(300);
    let server = server(&dir, own_uid(), deadline);
    let lock = rusqlite::Connection::open(dir.path("journal.sqlite3")).unwrap();
    lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let body = json!({"session": "chat"}).to_string();
    let (raw, elapsed) = serve_pair(&server, &request("status", body.as_bytes()));
    let response = parse(&raw);
    assert_eq!(response.status, 500, "{}", response.head);
    // The journal's own lock wait is 10 s.
    assert!(elapsed < deadline * 2, "{elapsed:?}");
    lock.execute_batch("ROLLBACK").unwrap();
    let (raw, _) = serve_pair(&server, &request("status", body.as_bytes()));
    assert_eq!(parse(&raw).status, 200);
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

/// The answer of an `inject` into a busy owner session.
fn busy() -> InjectResponse {
    InjectResponse {
        accepted: false,
        session_id: None,
        retry_after_s: Some(RETRY_AFTER_S),
        turn: None,
    }
}

#[test]
fn an_inject_into_the_free_owner_session_runs_and_journals_the_event_as_run_event_does() {
    let (fake, dir, engine) = setup(vec![text_step("noted")]);
    let s = &engine.socket;
    let hold = open_inbox(s);
    let answer = inject(s, "hive:h_1:done", 1);
    assert!(answer.accepted);
    assert_eq!(answer.session_id.as_deref(), Some(OWNER));
    assert_eq!(answer.retry_after_s, None);
    let turn = answer.turn.unwrap();
    assert_eq!(read_inbox(hold).turn.as_ref(), Some(&turn));
    assert_eq!(ended(s, OWNER, &turn).outcome, Some(TurnOutcome::Completed));
    assert_eq!(status(s, OWNER, None).last_turn.unwrap().turn, turn);
    let _hold = hold_inbox(s, OWNER);
    assert!(is_idle(s).idle);

    let twin = FakeGateway::start(vec![text_step("noted")]);
    let other = Scratch::new();
    let path = config(&other.0, twin.socket(), &other.path("c6.sock"));
    let out = command(
        &other.0,
        &[
            "run",
            "--session",
            OWNER,
            "--route",
            "main",
            "--prompt",
            "RESULT of hive:h_1:done.",
            "--event",
            "--config",
            path.to_str().unwrap(),
        ],
    )
    .output()
    .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let records = |dir: &Scratch| {
        Journal::open(&dir.path("journal.sqlite3"))
            .unwrap()
            .records(1)
            .unwrap()
    };
    assert_eq!(records(&dir), records(&other));
    // The system prompt names the working directory and the request id has its run's tag,
    // which differ.
    let sent = |fake: &FakeGateway| {
        let mut body: Value = serde_json::from_slice(&turns(fake)[0]).unwrap();
        body.as_object_mut().unwrap().remove("system");
        let id = untagged(body["request_id"].as_str().unwrap()).to_owned();
        body["request_id"] = Value::from(id);
        body
    };
    assert_eq!(sent(&fake), sent(&twin));
}

#[test]
fn a_busy_owner_session_refuses_an_inject_and_a_prompt_queues_behind_an_injected_turn() {
    let (fake, _dir, engine) = setup(vec![
        held("busy"),
        text_step("more"),
        held("event"),
        text_step("behind"),
    ]);
    let s = &engine.socket;
    let _hold = open_inbox(s);
    let first = prompt(s, OWNER, "work");
    assert!(fake.wait_hanging("s1-t1-"));
    assert_eq!(inject(s, "e1", 1), busy(), "a turn runs");
    let more = prompt(s, OWNER, "more");
    assert_eq!(inject(s, "e1", 1), busy(), "a turn runs and one is queued");
    assert_eq!(status(s, OWNER, None).queue_depth, 1);
    fake.release();
    ended(s, OWNER, &first);
    ended(s, OWNER, &more);

    let event = inject(s, "e1", 1);
    assert!(event.accepted);
    assert!(fake.wait_hanging("s1-t3-"));
    let behind = submit(s, &json!({"session": OWNER, "prompt": "behind"}));
    assert_eq!((behind.position, behind.queue_depth), (Some(1), 1));
    assert_eq!(
        report(s, OWNER, event.turn.as_deref().unwrap()).state,
        TurnState::Running
    );
    fake.release();
    ended(s, OWNER, &behind.turn.unwrap());
    let prompts: Vec<String> = turns(&fake).iter().map(|b| last_prompt(b)).collect();
    assert_eq!(prompts, ["work", "more", "RESULT of e1.", "behind"]);
}

/// The report of a turn the engine does not hold.
fn unknown(turn: &str) -> TurnReport {
    TurnReport {
        turn: turn.to_owned(),
        state: TurnState::Unknown,
        outcome: None,
        ended: None,
        line: None,
        delivery: None,
    }
}

#[test]
fn a_later_claim_answers_the_recorded_turn_while_it_runs_and_after_a_delivery_other_than_silent() {
    let (fake, _dir, engine) = setup(vec![held("one")]);
    let s = &engine.socket;
    let hold = open_inbox(s);
    let first = inject(s, "e1", 1);
    assert!(first.accepted);
    let turn = first.turn.clone().unwrap();
    assert_eq!(read_inbox(hold).turn.as_ref(), Some(&turn));
    assert!(fake.wait_hanging("s1-t1-"));
    assert_eq!(inject(s, "e1", 1), first, "a replay while the turn runs");
    assert_eq!(
        inject(s, "e1", 2),
        first,
        "a later claim while the turn runs"
    );
    fake.release();
    ended(s, OWNER, &turn);

    let _hold = hold_inbox(s, OWNER);
    assert_eq!(
        inject(s, "e1", 3),
        first,
        "a later claim before the delivery"
    );
    assert!(report_delivery(s, OWNER, &turn, "sent").ok);
    assert_eq!(inject(s, "e1", 4), first, "a later claim after `sent`");
    let followed = report(s, OWNER, &turn);
    assert_eq!(
        (followed.outcome, followed.delivery),
        (Some(TurnOutcome::Completed), Some(Delivery::Sent))
    );
    assert!(is_idle(s).idle, "nothing started");
    assert_eq!(turns(&fake).len(), 1);
}

#[test]
fn a_replay_after_a_restart_answers_the_recorded_turn_killed_while_running_and_starts_nothing() {
    let fake = FakeGateway::start(vec![held("never")]);
    let dir = Scratch::new();
    let config = config(&dir.0, fake.socket(), &dir.path("c6.sock"));
    let engine = start(&dir.0, &config);
    let _hold = open_inbox(&engine.socket);
    let before = inject(&engine.socket, "e1", 1);
    assert!(fake.wait_hanging("s1-t1-"));
    drop(engine);

    let engine = start(&dir.0, &config);
    let s = &engine.socket;
    let _hold = hold_inbox(s, OWNER);
    assert_eq!(inject(s, "e1", 1), before);
    let turn = before.turn.unwrap();
    assert_eq!(report(s, OWNER, &turn), unknown(&turn));
    assert!(is_idle(s).idle, "nothing started");
    assert_eq!(turns(&fake).len(), 1);
    assert_eq!(journaled_prompts(&dir, 1), []);
}

#[test]
fn a_later_claim_after_a_restart_answers_the_recorded_ended_turn_and_starts_nothing() {
    let fake = FakeGateway::start(vec![text_step("one")]);
    let dir = Scratch::new();
    let config = config(&dir.0, fake.socket(), &dir.path("c6.sock"));
    let engine = start(&dir.0, &config);
    let _hold = open_inbox(&engine.socket);
    let before = inject(&engine.socket, "e1", 1);
    let turn = before.turn.clone().unwrap();
    ended(&engine.socket, OWNER, &turn);
    drop(engine);

    let engine = start(&dir.0, &config);
    let s = &engine.socket;
    let _hold = hold_inbox(s, OWNER);
    assert_eq!(inject(s, "e1", 2), before);
    assert_eq!(inject(s, "e1", 1), before, "a replay of the first attempt");
    assert_eq!(report(s, OWNER, &turn), unknown(&turn));
    assert!(is_idle(s).idle, "nothing started");
    assert_eq!(turns(&fake).len(), 1);
    assert_eq!(
        journaled_prompts(&dir, 1),
        [(1, "RESULT of e1.".to_owned())]
    );
}

#[test]
fn a_record_whose_turn_never_started_answers_that_turn_which_reads_unknown() {
    let fake = FakeGateway::start(Vec::new());
    let dir = Scratch::new();
    let config = config(&dir.0, fake.socket(), &dir.path("c6.sock"));
    let never = "1700000000000000-1";
    let record = InboxDelivery {
        event_id: "e1".to_owned(),
        attempt: 1,
        turn: never.to_owned(),
        delivery: None,
    };
    Journal::open(&dir.path("journal.sqlite3"))
        .unwrap()
        .record_delivery(&record, READ_TIMEOUT)
        .unwrap();

    let engine = start(&dir.0, &config);
    let s = &engine.socket;
    let _hold = open_inbox(s);
    let answer = InjectResponse {
        accepted: true,
        session_id: Some(OWNER.to_owned()),
        retry_after_s: None,
        turn: Some(never.to_owned()),
    };
    assert_eq!(inject(s, "e1", 1), answer, "a replay");
    assert_eq!(inject(s, "e1", 2), answer, "a later claim");
    assert_eq!(report(s, OWNER, never), unknown(never));
    assert!(is_idle(s).idle, "nothing started");
    assert_eq!(fake.requests().len(), 0);
}

#[test]
fn a_higher_attempt_after_a_silent_delivery_starts_the_re_offer_before_and_after_a_restart() {
    let fake = FakeGateway::start(vec![text_step("one"), text_step("two"), text_step("three")]);
    let dir = Scratch::new();
    let config = config(&dir.0, fake.socket(), &dir.path("c6.sock"));
    let engine = start(&dir.0, &config);
    let s = &engine.socket;
    let offer = |attempt: u64, delivery: &str| {
        let hold = hold_inbox(s, OWNER);
        let answer = inject(s, "e1", attempt);
        assert!(answer.accepted, "{attempt}");
        let turn = answer.turn.clone().unwrap();
        assert_eq!(read_inbox(hold).turn.as_ref(), Some(&turn));
        ended(s, OWNER, &turn);
        assert!(report_delivery(s, OWNER, &turn, delivery).ok);
        answer
    };
    set_route(s, OWNER, "main");
    let first = offer(1, "silent");
    assert_eq!(inject(s, "e1", 1), first, "a replay of the silent offer");
    let second = offer(2, "silent");
    assert_ne!(second.turn, first.turn);
    drop(engine);

    let engine = start(&dir.0, &config);
    let s = &engine.socket;
    let hold = hold_inbox(s, OWNER);
    let third = inject(s, "e1", 3);
    assert!(third.accepted);
    let turn = third.turn.clone().unwrap();
    assert_ne!(third.turn, second.turn);
    assert_eq!(read_inbox(hold).turn.as_ref(), Some(&turn));
    ended(s, OWNER, &turn);
    assert!(report_delivery(s, OWNER, &turn, "silent_done").ok);
    let _hold = hold_inbox(s, OWNER);
    assert_eq!(inject(s, "e1", 4), third, "`silent_done` settles the event");
    assert_eq!(turns(&fake).len(), 3);
    assert_eq!(
        journaled_prompts(&dir, 1),
        [
            (1, "RESULT of e1.".to_owned()),
            (2, "RESULT of e1.".to_owned()),
            (3, "RESULT of e1.".to_owned())
        ]
    );
}

/// Asserts that the owner session is not free: `is_idle` answers busy and an `inject` is
/// refused with nothing started.
fn not_free(s: &std::path::Path, fake: &FakeGateway) {
    assert_eq!(
        is_idle(s),
        IdleStatus {
            idle: false,
            since: None
        }
    );
    let sent = turns(fake).len();
    assert_eq!(inject(s, "refused", 1), busy());
    let owner = status(s, OWNER, None);
    assert_eq!((owner.running, owner.queue_depth), (false, 0));
    assert_eq!(turns(fake).len(), sent);
}

/// Waits at most 10 s until `is_idle` answers busy.
fn until_busy(s: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while is_idle(s).idle {
        assert!(Instant::now() < deadline, "still idle");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn an_owner_key_without_a_journal_session_is_not_free_though_inbox_turns_is_held() {
    let (fake, _dir, engine) = setup(vec![text_step("noted")]);
    let s = &engine.socket;
    let hold = hold_inbox(s, OWNER);
    not_free(s, &fake);
    assert_eq!(status(s, OWNER, None).last_turn, None);

    set_route(s, OWNER, "main");
    assert!(is_idle(s).idle);
    let turn = inject(s, "e1", 1).turn.unwrap();
    assert_eq!(read_inbox(hold).turn, Some(turn));
}

#[test]
fn an_owner_session_without_a_held_inbox_turns_is_not_free() {
    let (fake, _dir, engine) = setup(vec![text_step("noted")]);
    let s = &engine.socket;
    set_route(s, OWNER, "main");
    not_free(s, &fake);

    let hold = hold_inbox(s, OWNER);
    assert!(is_idle(s).idle);
    let turn = inject(s, "e1", 1).turn.unwrap();
    assert_eq!(read_inbox(hold).turn.as_ref(), Some(&turn));
    ended(s, OWNER, &turn);
    not_free(s, &fake);
}

#[test]
fn inbox_turns_for_a_key_other_than_the_owner_session_answers_400_and_holds_nothing() {
    let (fake, _dir, engine) = setup(Vec::new());
    let s = &engine.socket;
    set_route(s, OWNER, "main");
    for body in [json!({"session": "chat"}), json!({})] {
        let response = exchange(s, &request("inbox_turns", body.to_string().as_bytes()));
        let error: WireError = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(
            (response.status, error.code),
            (400, ErrorCode::BadRequest),
            "{body}"
        );
    }
    not_free(s, &fake);
}

#[test]
fn a_held_inbox_turns_is_answered_with_the_turn_inject_starts() {
    let (_fake, _dir, engine) = setup(vec![held("noted")]);
    let s = &engine.socket;
    let hold = open_inbox(s);
    waiting(&hold);
    let turn = inject(s, "e1", 1).turn.unwrap();
    assert_eq!(read_inbox(hold).turn.as_ref(), Some(&turn));
    assert_eq!(report(s, OWNER, &turn).state, TurnState::Running);
}

#[test]
fn a_new_inbox_turns_releases_the_held_one_at_once_with_no_turn() {
    let (_fake, _dir, engine) = setup(vec![text_step("noted")]);
    let s = &engine.socket;
    let first = open_inbox(s);
    waiting(&first);
    let second = hold_inbox(s, OWNER);
    assert_eq!(read_inbox(first), InboxTurns { turn: None });
    waiting(&second);
    let turn = inject(s, "e1", 1).turn.unwrap();
    assert_eq!(read_inbox(second).turn, Some(turn));
}

#[test]
fn an_inbox_turn_whose_answer_fails_to_write_is_answered_at_once_to_the_next_request() {
    let (_fake, _dir, engine) = setup(vec![text_step("noted")]);
    let s = &engine.socket;
    drop(open_inbox(s));
    let turn = inject(s, "e1", 1).turn.unwrap();
    ended(s, OWNER, &turn);
    // The hold ends when its answer is written, so a busy session shows the write was tried.
    until_busy(s);
    assert!(
        !report_delivery(s, OWNER, &turn, "sent").ok,
        "not handed over"
    );

    assert_eq!(read_inbox(hold_inbox(s, OWNER)).turn.as_ref(), Some(&turn));
    assert!(!is_idle(s).idle, "an answered request holds nothing");
    let again = hold_inbox(s, OWNER);
    waiting(&again);
    assert!(report_delivery(s, OWNER, &turn, "sent").ok);
}

#[test]
fn report_delivery_stores_one_delivery_on_an_ended_handed_over_inbox_turn() {
    let (fake, _dir, engine) = setup(vec![held("noted"), text_step("hello")]);
    let s = &engine.socket;
    let hold = open_inbox(s);
    let turn = inject(s, "e1", 1).turn.unwrap();
    assert_eq!(read_inbox(hold).turn.as_ref(), Some(&turn));
    assert!(fake.wait_hanging("s1-t1-"));
    assert!(!report_delivery(s, OWNER, &turn, "sent").ok, "running");
    fake.release();
    assert_eq!(ended(s, OWNER, &turn).delivery, None);

    assert_eq!(
        report_delivery(s, OWNER, &turn, "silent_done"),
        Ack {
            ok: true,
            message: None
        }
    );
    assert!(
        !report_delivery(s, OWNER, &turn, "sent").ok,
        "a second report"
    );
    assert_eq!(report(s, OWNER, &turn).delivery, Some(Delivery::SilentDone));

    let submitted = prompt(s, OWNER, "hi");
    ended(s, OWNER, &submitted);
    assert!(
        !report_delivery(s, OWNER, &submitted, "sent").ok,
        "submitted"
    );
    assert!(!report_delivery(s, OWNER, "1-999", "sent").ok, "unknown");
    assert!(
        !report_delivery(s, "chat", &turn, "sent").ok,
        "another session"
    );
    assert_eq!(report(s, OWNER, &submitted).delivery, None);
    assert_eq!(
        report(s, OWNER, &turn).delivery,
        Some(Delivery::SilentDone),
        "kept with the report"
    );
}

/// Holds `inbox_turns` for the owner session on one end of a socketpair; the other end is
/// returned.
fn hold_pair(server: &Server) -> UnixStream {
    let (mut client, end) = UnixStream::pair().unwrap();
    let body = json!({"session": OWNER}).to_string();
    client
        .write_all(&request("inbox_turns", body.as_bytes()))
        .unwrap();
    server.serve(&end);
    client
}

#[test]
fn the_delivery_of_an_inbox_turn_is_kept_as_long_as_its_report() {
    let dir = Scratch::new();
    let server = server(&dir, own_uid(), READ_TIMEOUT);
    let call = |method: &str, body: Value| {
        let (raw, _) = serve_pair(&server, &request(method, body.to_string().as_bytes()));
        parse(&raw).body
    };
    let report = |id: &str| {
        let status: SessionStatus =
            serde_json::from_slice(&call("status", json!({"session": OWNER, "turn": id}))).unwrap();
        status.turn.unwrap()
    };
    call("set_route", json!({"session": OWNER, "route": "main"}));
    let hold = hold_pair(&server);
    let injected: InjectResponse = serde_json::from_slice(&call(
        "inject",
        json!({"event_id": "e1", "kind": "task_result", "prompt": "RESULT of e1.",
               "lease": "0123abcd", "attempt": 1}),
    ))
    .unwrap();
    let turn = injected.turn.unwrap();
    assert_eq!(read_inbox(hold).turn.as_ref(), Some(&turn));
    until_ended(&turn, || report(&turn));
    let ack: Ack = serde_json::from_slice(&call(
        "report_delivery",
        json!({"session": OWNER, "turn": turn, "delivery": "sent"}),
    ))
    .unwrap();
    assert!(ack.ok);

    for n in 1..=KEPT_REPORTS {
        assert_eq!(report(&turn).delivery, Some(Delivery::Sent), "{n}");
        let answer: SubmitTurnResponse = serde_json::from_slice(&call(
            "submit_turn",
            json!({"session": OWNER, "prompt": format!("turn {n}")}),
        ))
        .unwrap();
        let id = answer.turn.unwrap();
        until_ended(&id, || report(&id));
    }
    let gone = report(&turn);
    assert_eq!((gone.state, gone.delivery), (TurnState::Unknown, None));
}

#[test]
fn a_stored_route_reaches_the_queued_and_later_turns_and_the_running_turn_keeps_its_own() {
    let (fake, _dir, engine) = setup(vec![
        Answer::Held(vec![
            route(),
            call(
                "c1",
                "search_files",
                json!({"pattern": "*", "target": "files"}),
            ),
            stop(StopReason::ToolUse),
        ]),
        text_step("first"),
        text_step("second"),
        text_step("third"),
    ]);
    let s = &engine.socket;
    let first = prompt(s, "chat", "first");
    assert!(fake.wait_hanging("s1-t1-"));
    let second = prompt(s, "chat", "second");
    assert_eq!(
        set_route(s, "chat", "deep"),
        RouteSet {
            session: "chat".to_owned(),
            route: "deep".to_owned(),
        }
    );
    assert_eq!(status(s, "chat", None).route, "deep");
    assert_eq!(status(s, "other", None).route, "main");

    fake.release();
    ended(s, "chat", &second);
    let third = prompt(s, "chat", "third");
    ended(s, "chat", &third);
    assert_eq!(
        ended(s, "chat", &first).outcome,
        Some(TurnOutcome::Completed)
    );
    assert_eq!(routes(&fake), ["main", "main", "deep", "deep"]);
}

#[test]
fn a_stored_route_survives_a_restart_and_a_new_session_starts_on_the_default() {
    let fake = FakeGateway::start(vec![text_step("one"), text_step("two")]);
    let dir = Scratch::new();
    let config = config(&dir.0, fake.socket(), &dir.path("c6.sock"));
    let engine = start(&dir.0, &config);
    set_route(&engine.socket, "chat", "deep");
    drop(engine);

    let engine = start(&dir.0, &config);
    let s = &engine.socket;
    assert_eq!(status(s, "chat", None).route, "deep");
    let turn = prompt(s, "chat", "first");
    ended(s, "chat", &turn);
    let fresh = submit(
        s,
        &json!({"session": "chat", "prompt": "fresh", "new": true}),
    );
    ended(s, "chat", &fresh.turn.unwrap());
    assert_eq!(routes(&fake), ["deep", "main"]);
    assert_eq!(status(s, "chat", None).route, "main");
    let journal = Journal::open(&dir.path("journal.sqlite3")).unwrap();
    assert_eq!(journal.route("chat", READ_TIMEOUT).unwrap(), None);
}

#[test]
fn an_alias_the_gateway_refuses_fails_the_turn_and_stays_stored() {
    let mut unknown = gateway_error(ErrorClass::BadRequest);
    unknown.message = Some("unknown route alias".to_owned());
    let (fake, _dir, engine) = setup(vec![Answer::Reject(400, unknown), text_step("back")]);
    let s = &engine.socket;
    set_route(s, "chat", "nowhere");
    let turn = prompt(s, "chat", "hi");
    let report = ended(s, "chat", &turn);
    assert_eq!(report.outcome, Some(TurnOutcome::Failed));
    assert!(report.line.is_some());
    assert_eq!(status(s, "chat", None).route, "nowhere");

    set_route(s, "chat", "main");
    let again = prompt(s, "chat", "again");
    assert_eq!(
        ended(s, "chat", &again).outcome,
        Some(TurnOutcome::Completed)
    );
    assert_eq!(routes(&fake), ["nowhere", "main"]);
}

#[test]
fn an_unanswered_qb_cancel_holds_an_interrupted_turn_at_most_the_cancel_timeout() {
    let (fake, _dir, engine) = setup(vec![held("never")]);
    let s = &engine.socket;
    let turn = prompt(s, "chat", "first");
    assert!(fake.wait_hanging("s1-t1-"));
    fake.hold_cancels();
    let started = Instant::now();
    assert!(interrupt(s, "chat").interrupted);
    cancel_arrived(&fake);
    assert_eq!(
        ended(s, "chat", &turn).outcome,
        Some(TurnOutcome::Interrupted)
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed >= CANCEL_TIMEOUT && elapsed < CANCEL_TIMEOUT + Duration::from_secs(1),
        "{elapsed:?}"
    );
}

/// The first turn finishes its handle and then waits for a release, as a turn does between
/// the runner's return and the server's report; later turns return at once. Every handle is
/// kept.
struct Lingering {
    finished: Mutex<SyncSender<()>>,
    release: Mutex<Receiver<()>>,
    handles: Mutex<Vec<Interrupt>>,
}

impl Runner for Lingering {
    fn run(&self, _: &Input, interrupt: &Interrupt, _: &mut dyn FnMut(&str)) -> Ending {
        let first = {
            let mut handles = self.handles.lock().unwrap();
            handles.push(interrupt.clone());
            handles.len() == 1
        };
        if first {
            interrupt.finish();
            self.finished.lock().unwrap().send(()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
        }
        Ending {
            outcome: TurnOutcome::Completed,
            line: None,
            answer: None,
        }
    }
}

#[test]
fn an_interrupt_after_the_turn_finished_neither_claims_it_nor_drops_the_queue_head() {
    let dir = Scratch::new();
    let (finished, on_finish) = mpsc::sync_channel(1);
    let (release, on_release) = mpsc::sync_channel(1);
    let runner = Arc::new(Lingering {
        finished: Mutex::new(finished),
        release: Mutex::new(on_release),
        handles: Mutex::new(Vec::new()),
    });
    let server = server_with(&dir, runner.clone(), own_uid(), READ_TIMEOUT);
    let call = |method: &str, body: Value| {
        let (raw, _) = serve_pair(&server, &request(method, body.to_string().as_bytes()));
        parse(&raw).body
    };
    let submit = |prompt: &str| -> SubmitTurnResponse {
        serde_json::from_slice(&call(
            "submit_turn",
            json!({"session": "chat", "prompt": prompt}),
        ))
        .unwrap()
    };
    let first = submit("first").turn.unwrap();
    on_finish.recv_timeout(Duration::from_secs(5)).unwrap();
    let queued = submit("second").turn.unwrap();

    let late: InterruptResponse =
        serde_json::from_slice(&call("interrupt", json!({"session": "chat"}))).unwrap();
    assert_eq!(
        late,
        InterruptResponse {
            interrupted: false,
            turn: None,
            dropped: None,
            queue_depth: 1,
        }
    );
    release.send(()).unwrap();
    let state = |id: &str| {
        until_ended(id, || {
            let status: SessionStatus =
                serde_json::from_slice(&call("status", json!({"session": "chat", "turn": id})))
                    .unwrap();
            status.turn.unwrap()
        })
    };
    assert_eq!(state(&first).outcome, Some(TurnOutcome::Completed));
    assert_eq!(state(&queued).outcome, Some(TurnOutcome::Completed));
    let handles = runner.handles.lock().unwrap();
    assert_eq!(handles.len(), 2);
    assert!(
        !handles[1].interrupt(),
        "the server finished the handle once the runner returned"
    );
}

/// The first turn decides `completed` and then waits for a release before it returns, as a
/// turn does between its outcome and the server's report; later turns return at once.
struct Deciding {
    decided: Mutex<SyncSender<()>>,
    release: Mutex<Receiver<()>>,
    runs: Mutex<usize>,
}

impl Runner for Deciding {
    fn run(&self, _: &Input, _: &Interrupt, _: &mut dyn FnMut(&str)) -> Ending {
        let first = {
            let mut runs = self.runs.lock().unwrap();
            *runs += 1;
            *runs == 1
        };
        if first {
            self.decided.lock().unwrap().send(()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
        }
        Ending {
            outcome: TurnOutcome::Completed,
            line: None,
            answer: None,
        }
    }
}

#[test]
fn a_turn_an_interrupt_claims_after_its_runner_decided_ends_interrupted() {
    let dir = Scratch::new();
    let (decided, on_decided) = mpsc::sync_channel(1);
    let (release, on_release) = mpsc::sync_channel(1);
    let runner = Arc::new(Deciding {
        decided: Mutex::new(decided),
        release: Mutex::new(on_release),
        runs: Mutex::new(0),
    });
    let server = server_with(&dir, runner, own_uid(), READ_TIMEOUT);
    let call = |method: &str, body: Value| {
        let (raw, _) = serve_pair(&server, &request(method, body.to_string().as_bytes()));
        parse(&raw).body
    };
    let submit = |prompt: &str| -> String {
        let answer: SubmitTurnResponse = serde_json::from_slice(&call(
            "submit_turn",
            json!({"session": "chat", "prompt": prompt}),
        ))
        .unwrap();
        answer.turn.unwrap()
    };
    let first = submit("first");
    on_decided.recv_timeout(Duration::from_secs(5)).unwrap();
    let head = submit("second");

    let claimed: InterruptResponse =
        serde_json::from_slice(&call("interrupt", json!({"session": "chat"}))).unwrap();
    assert_eq!(
        claimed,
        InterruptResponse {
            interrupted: true,
            turn: Some(first.clone()),
            dropped: Some(head.clone()),
            queue_depth: 0,
        }
    );
    release.send(()).unwrap();
    let report = until_ended(&first, || {
        let status: SessionStatus =
            serde_json::from_slice(&call("status", json!({"session": "chat", "turn": first})))
                .unwrap();
        status.turn.unwrap()
    });
    assert_eq!(report.outcome, Some(TurnOutcome::Interrupted), "{report:?}");
    assert_eq!(report.line, None);
}

/// Hands over one interim text, waits until the turn is interrupted, hands over another and
/// answers.
struct Narrating;

impl Runner for Narrating {
    fn run(&self, _: &Input, interrupt: &Interrupt, interim: &mut dyn FnMut(&str)) -> Ending {
        interim("before");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !interrupt.is_interrupted() {
            assert!(Instant::now() < deadline, "no interrupt");
            std::thread::sleep(Duration::from_millis(5));
        }
        interim("after");
        Ending {
            outcome: TurnOutcome::Completed,
            line: None,
            answer: Some("late".to_owned()),
        }
    }
}

#[test]
fn an_interim_handed_over_after_the_interrupt_fired_is_not_appended() {
    let dir = Scratch::new();
    let server = server_with(&dir, Arc::new(Narrating), own_uid(), READ_TIMEOUT);
    let call = |method: &str, body: Value| {
        let (raw, _) = serve_pair(&server, &request(method, body.to_string().as_bytes()));
        parse(&raw).body
    };
    let answer: SubmitTurnResponse = serde_json::from_slice(&call(
        "submit_turn",
        json!({"session": "chat", "prompt": "hi"}),
    ))
    .unwrap();
    let turn = answer.turn.unwrap();
    let page = |after: u64| -> TurnEventsPage {
        serde_json::from_slice(&call(
            "turn_events",
            json!({"session": "chat", "turn": turn, "after": after}),
        ))
        .unwrap()
    };
    assert_eq!(
        events(&page(1)),
        json!([{"seq": 2, "type": "interim", "text": "before"}])
    );
    let fired: InterruptResponse =
        serde_json::from_slice(&call("interrupt", json!({"session": "chat"}))).unwrap();
    assert!(fired.interrupted);
    assert_eq!(
        events(&page(2)),
        json!([{"seq": 3, "type": "end", "outcome": "interrupted"}])
    );
}

#[test]
fn a_completed_turn_has_start_final_and_end_in_order_and_so_has_an_injected_turn() {
    let (_fake, _dir, engine) = setup(vec![text_step("hello"), text_step("noted")]);
    let s = &engine.socket;
    let turn = prompt(s, "chat", "hi");
    ended(s, "chat", &turn);

    let page = turn_events(s, "chat", &turn, 0);
    assert_eq!(
        (page.turn.as_str(), page.state),
        (turn.as_str(), TurnState::Ended)
    );
    assert_eq!(
        events(&page),
        json!([
            {"seq": 1, "type": "start"},
            {"seq": 2, "type": "final", "text": "hello"},
            {"seq": 3, "type": "end", "outcome": "completed"},
        ])
    );
    assert_eq!(
        events(&turn_events(s, "chat", &turn, 2)),
        json!([{"seq": 3, "type": "end", "outcome": "completed"}])
    );
    let past = turn_events(s, "chat", &turn, 3);
    assert_eq!((past.state, past.events), (TurnState::Ended, Vec::new()));

    let _hold = open_inbox(s);
    let event = inject(s, "e1", 1).turn.unwrap();
    ended(s, OWNER, &event);
    assert_eq!(
        events(&turn_events(s, OWNER, &event, 0)),
        json!([
            {"seq": 1, "type": "start"},
            {"seq": 2, "type": "final", "text": "noted"},
            {"seq": 3, "type": "end", "outcome": "completed"},
        ])
    );
}

#[test]
fn a_turn_that_ends_without_answer_text_has_no_final() {
    let (_fake, _dir, engine) = setup(vec![Answer::Events(vec![
        route(),
        usage(0),
        stop(StopReason::MaxTokens),
    ])]);
    let s = &engine.socket;
    let turn = prompt(s, "chat", "hi");
    assert_eq!(
        ended(s, "chat", &turn).outcome,
        Some(TurnOutcome::Completed)
    );
    assert_eq!(
        events(&turn_events(s, "chat", &turn, 0)),
        json!([
            {"seq": 1, "type": "start"},
            {"seq": 2, "type": "end", "outcome": "completed"},
        ])
    );
}

#[test]
fn quota_context_and_failed_turns_end_with_their_report_line_and_no_final() {
    let mut quota = gateway_error(ErrorClass::QuotaExhausted);
    quota.resets_at = Some(1_700_000_000);
    let (_fake, _dir, engine) = setup(vec![
        Answer::Reject(429, quota),
        Answer::Events(vec![
            route(),
            error_event(gateway_error(ErrorClass::ContextTooLong)),
        ]),
        Answer::Reject(400, gateway_error(ErrorClass::BadRequest)),
    ]);
    let s = &engine.socket;
    for (session, outcome) in [
        ("quota", TurnOutcome::QuotaExhausted),
        ("context", TurnOutcome::ContextExhausted),
        ("failed", TurnOutcome::Failed),
    ] {
        let turn = prompt(s, session, "hi");
        let report = ended(s, session, &turn);
        assert_eq!(report.outcome, Some(outcome));
        let line = report.line.unwrap();
        assert_eq!(
            events(&turn_events(s, session, &turn, 0)),
            json!([
                {"seq": 1, "type": "start"},
                {"seq": 2, "type": "end", "outcome": outcome.as_str(), "line": line},
            ]),
            "{session}"
        );
    }
}

#[test]
fn a_route_without_a_candidate_fails_the_turn_with_its_class_named() {
    let (_fake, _dir, engine) = setup(vec![
        Answer::Events(vec![error_event(gateway_error(ErrorClass::NoRoute))]),
        Answer::Reject(400, gateway_error(ErrorClass::NoRoute)),
    ]);
    let s = &engine.socket;
    for session in ["stream", "rejected"] {
        let turn = prompt(s, session, "hi");
        let report = ended(s, session, &turn);
        assert_eq!(report.outcome, Some(TurnOutcome::Failed), "{session}");
        assert_eq!(
            report.line.as_deref(),
            Some("ulei-engine: gateway error no_route: no message"),
            "{session}"
        );
    }
}

#[test]
fn an_interrupt_during_a_running_tool_ends_the_turn_without_final_and_drops_the_queued_one() {
    let (fake, dir, engine) = setup(vec![
        tool_step(vec![call(
            "c1",
            "terminal",
            json!({"command": "echo $$ > shell.pid; sleep 30"}),
        )]),
        text_step("unreachable"),
    ]);
    let s = &engine.socket;
    let first = prompt(s, "chat", "run it");
    pid_file(&dir.path("shell.pid"));
    let second = prompt(s, "chat", "next");
    let held = connect(s, &events_request("chat", &second, 0));

    let answer = interrupt(s, "chat");
    assert_eq!(answer.dropped.as_deref(), Some(second.as_str()));
    let dropped = read_page(held);
    assert_eq!(dropped.state, TurnState::Dropped);
    assert_eq!(events(&dropped), json!([{"seq": 1, "type": "dropped"}]));
    assert_eq!(
        ended(s, "chat", &first).outcome,
        Some(TurnOutcome::Interrupted)
    );
    assert_eq!(
        events(&turn_events(s, "chat", &first, 0)),
        json!([
            {"seq": 1, "type": "start"},
            {"seq": 2, "type": "end", "outcome": "interrupted"},
        ])
    );
    assert_eq!(turns(&fake).len(), 1);
}

#[test]
fn a_tool_step_gives_an_interim_with_its_visible_text_before_final_and_end_in_every_turn_kind() {
    let (_fake, _dir, engine) = setup(vec![
        Answer::Events(vec![
            route(),
            thinking("opaque"),
            text("\n<think>I should list the files.</think>\nLet me list the files.  "),
            call(
                "c1",
                "search_files",
                json!({"pattern": "*", "target": "files"}),
            ),
            stop(StopReason::ToolUse),
        ]),
        text_step("done"),
        narrated("Checking the event.", "c2"),
        text_step("noted"),
    ]);
    let s = &engine.socket;
    let turn = prompt(s, "chat", "list them");
    ended(s, "chat", &turn);
    assert_eq!(
        events(&turn_events(s, "chat", &turn, 0)),
        json!([
            {"seq": 1, "type": "start"},
            {"seq": 2, "type": "interim", "text": "Let me list the files."},
            {"seq": 3, "type": "final", "text": "done"},
            {"seq": 4, "type": "end", "outcome": "completed"},
        ])
    );

    let _hold = open_inbox(s);
    let event = inject(s, "e1", 1).turn.unwrap();
    ended(s, OWNER, &event);
    assert_eq!(
        events(&turn_events(s, OWNER, &event, 0)),
        json!([
            {"seq": 1, "type": "start"},
            {"seq": 2, "type": "interim", "text": "Checking the event."},
            {"seq": 3, "type": "final", "text": "noted"},
            {"seq": 4, "type": "end", "outcome": "completed"},
        ])
    );
}

#[test]
fn empty_placeholder_and_whitespace_duplicate_interim_texts_are_not_sent() {
    let (_fake, _dir, engine) = setup(vec![
        narrated(" \n ", "c1"),
        narrated("(empty)", "c2"),
        narrated("Reading  the\nfiles.", "c3"),
        narrated(" Reading the files. ", "c4"),
        narrated("Reading the\tfiles.", "c5"),
        text_step("done"),
    ]);
    let s = &engine.socket;
    let turn = prompt(s, "chat", "read them");
    ended(s, "chat", &turn);
    assert_eq!(
        events(&turn_events(s, "chat", &turn, 0)),
        json!([
            {"seq": 1, "type": "start"},
            {"seq": 2, "type": "interim", "text": "Reading  the\nfiles."},
            {"seq": 3, "type": "final", "text": "done"},
            {"seq": 4, "type": "end", "outcome": "completed"},
        ])
    );
}

#[test]
fn an_interrupt_during_a_tool_leaves_the_interim_before_it_and_none_or_final_after_it() {
    let (fake, dir, engine) = setup(vec![
        Answer::Events(vec![
            route(),
            text("Starting the long job."),
            call(
                "c1",
                "terminal",
                json!({"command": "echo $$ > shell.pid; sleep 30"}),
            ),
            stop(StopReason::ToolUse),
        ]),
        narrated("Never said.", "c2"),
        text_step("unreachable"),
    ]);
    let s = &engine.socket;
    let turn = prompt(s, "chat", "run it");
    pid_file(&dir.path("shell.pid"));
    assert!(interrupt(s, "chat").interrupted);
    assert_eq!(
        ended(s, "chat", &turn).outcome,
        Some(TurnOutcome::Interrupted)
    );
    assert_eq!(
        events(&turn_events(s, "chat", &turn, 0)),
        json!([
            {"seq": 1, "type": "start"},
            {"seq": 2, "type": "interim", "text": "Starting the long job."},
            {"seq": 3, "type": "end", "outcome": "interrupted"},
        ])
    );
    assert_eq!(turns(&fake).len(), 1);
}

#[test]
fn the_context_reaches_every_request_of_its_turn_after_the_prompt_and_before_a_policy_context() {
    let fake = FakeGateway::start(vec![
        narrated("Looking.", "c1"),
        text_step("again"),
        text_step("fine"),
    ]);
    let dir = Scratch::new();
    let config = config(&dir.0, fake.socket(), &dir.path("c6.sock"));
    let reminder = dir.path("reminder.md");
    fs::write(&reminder, "Stay brief.").unwrap();
    let settings = format!(
        "{}main_channel = \"main-chat\"\nturn_reminder_file = \"{}\"\nmax_steps = 1\n",
        fs::read_to_string(&config).unwrap(),
        reminder.display()
    );
    fs::write(&config, settings).unwrap();
    let engine = start(&dir.0, &config);
    let s = &engine.socket;
    let context = "Replying to message 7 of the thread.";

    let first = submit(
        s,
        &json!({"session": "chat", "prompt": "hi", "channel": "main-chat", "context": context}),
    );
    ended(s, "chat", &first.turn.unwrap());
    let second = submit(
        s,
        &json!({"session": "chat", "prompt": "next", "channel": "main-chat"}),
    );
    ended(s, "chat", &second.turn.unwrap());

    let sent: Vec<Vec<String>> = turns(&fake).iter().map(|b| user_texts(b)).collect();
    let input = format!("hi\n\n{context}\n\nStay brief.");
    assert_eq!(
        sent,
        [
            vec![input.clone()],
            vec![input, SUMMARY_REQUEST.to_owned()],
            vec![
                "hi".to_owned(),
                SUMMARY_REQUEST.to_owned(),
                "next\n\nStay brief.".to_owned(),
            ],
        ]
    );
    let journal = Journal::open(&dir.path("journal.sqlite3")).unwrap();
    let journaled: Vec<(u64, String)> = journal
        .records(1)
        .unwrap()
        .into_iter()
        .filter(|m| m.message["role"] == "user" && m.message["content"][0]["type"] == "text")
        .map(|m| (m.turn, m.message["content"][0]["text"].to_string()))
        .collect();
    assert_eq!(
        journaled,
        [
            (1, json!("hi").to_string()),
            (1, json!(SUMMARY_REQUEST).to_string()),
            (2, json!("next").to_string()),
        ]
    );
    let stored = fs::read(dir.path("journal.sqlite3")).unwrap();
    assert!(
        !stored
            .windows(context.len())
            .any(|w| w == context.as_bytes())
    );
}

#[test]
fn a_caller_that_disconnects_while_held_and_calls_again_with_the_same_after_gets_every_event() {
    let (fake, _dir, engine) = setup(vec![held("the answer")]);
    let s = &engine.socket;
    let turn = prompt(s, "chat", "hi");
    assert!(fake.wait_hanging("s1-t1-"));
    let first = turn_events(s, "chat", &turn, 0);
    assert_eq!(first.state, TurnState::Running);
    assert_eq!(events(&first), json!([{"seq": 1, "type": "start"}]));

    let gone = connect(s, &events_request("chat", &turn, 1));
    waiting(&gone);
    drop(gone);
    let again = connect(s, &events_request("chat", &turn, 1));
    waiting(&again);
    fake.release();
    let page = read_page(again);
    assert_eq!(page.state, TurnState::Ended);
    assert_eq!(
        events(&page),
        json!([
            {"seq": 2, "type": "final", "text": "the answer"},
            {"seq": 3, "type": "end", "outcome": "completed"},
        ])
    );
    assert_eq!(
        cancels(&fake),
        Vec::<String>::new(),
        "the turn was not cancelled"
    );
}

#[test]
fn status_submit_turn_and_interrupt_are_answered_while_a_turn_events_is_held() {
    let (fake, _dir, engine) = setup(vec![held("one"), text_step("two")]);
    let s = &engine.socket;
    let turn = prompt(s, "chat", "first");
    assert!(fake.wait_hanging("s1-t1-"));
    let held = connect(s, &events_request("chat", &turn, 1));
    waiting(&held);

    assert!(status(s, "chat", None).running);
    let queued = submit(s, &json!({"session": "chat", "prompt": "second"}));
    assert_eq!(queued.position, Some(1));
    assert!(!interrupt(s, "other").interrupted);
    waiting(&held);

    fake.release();
    assert_eq!(
        events(&read_page(held)),
        json!([
            {"seq": 2, "type": "final", "text": "one"},
            {"seq": 3, "type": "end", "outcome": "completed"},
        ])
    );
    ended(s, "chat", &queued.turn.unwrap());
}

#[test]
fn a_newer_request_for_a_turn_releases_the_held_one_at_once_with_what_it_has() {
    let (fake, _dir, engine) = setup(vec![held("done")]);
    let s = &engine.socket;
    let turn = prompt(s, "chat", "hi");
    assert!(fake.wait_hanging("s1-t1-"));
    let older = connect(s, &events_request("chat", &turn, 1));
    waiting(&older);

    let newer = connect(s, &events_request("chat", &turn, 1));
    let released = read_page(older);
    assert_eq!(
        (released.state, released.events),
        (TurnState::Running, Vec::new())
    );
    waiting(&newer);
    fake.release();
    assert_eq!(
        events(&read_page(newer)),
        json!([
            {"seq": 2, "type": "final", "text": "done"},
            {"seq": 3, "type": "end", "outcome": "completed"},
        ])
    );
}

/// Answers every turn at once as completed with as many `a` as its prompt says.
struct Sized;

impl Runner for Sized {
    fn run(&self, input: &Input, _: &Interrupt, _: &mut dyn FnMut(&str)) -> Ending {
        Ending {
            outcome: TurnOutcome::Completed,
            line: None,
            answer: Some("a".repeat(input.request.prompt.parse().unwrap())),
        }
    }
}

#[test]
fn a_page_carries_the_events_that_fit_512_kib_and_an_event_too_big_alone_answers_500() {
    let dir = Scratch::new();
    let server = server_with(&dir, Arc::new(Sized), own_uid(), READ_TIMEOUT);
    let call = |method: &str, body: Value| {
        let (raw, _) = serve_pair(&server, &request(method, body.to_string().as_bytes()));
        parse(&raw)
    };
    let finished = |length: usize| {
        let body = json!({"session": "chat", "prompt": length.to_string()});
        let answer: SubmitTurnResponse =
            serde_json::from_slice(&call("submit_turn", body).body).unwrap();
        let id = answer.turn.unwrap();
        until_ended(&id, || {
            let status: SessionStatus = serde_json::from_slice(
                &call("status", json!({"session": "chat", "turn": id})).body,
            )
            .unwrap();
            status.turn.unwrap()
        });
        id
    };
    let page = |id: &str, after: u64| {
        let response = call(
            "turn_events",
            json!({"session": "chat", "turn": id, "after": after}),
        );
        assert_eq!(response.status, 200, "{}", response.head);
        let page: TurnEventsPage = serde_json::from_slice(&response.body).unwrap();
        page.events
            .iter()
            .map(|e| (e.r#type, e.text.as_ref().map(String::len)))
            .collect::<Vec<_>>()
    };

    // A `final` of this text fits a page alone and not after `start`.
    let fits = finished(MAX_BODY - 100);
    assert_eq!(page(&fits, 0), [(TurnEventType::Start, None)]);
    assert_eq!(
        page(&fits, 1),
        [(TurnEventType::Final, Some(MAX_BODY - 100))]
    );
    assert_eq!(page(&fits, 2), [(TurnEventType::End, None)]);

    let too_big = finished(MAX_BODY);
    assert_eq!(page(&too_big, 0), [(TurnEventType::Start, None)]);
    let refused = call(
        "turn_events",
        json!({"session": "chat", "turn": too_big, "after": 1}),
    );
    let error: WireError = serde_json::from_slice(&refused.body).unwrap();
    assert_eq!((refused.status, error.code), (500, ErrorCode::Internal));
    assert_eq!(page(&too_big, 2), [(TurnEventType::End, None)]);
}

/// The `inject` request of event `e1` at `attempt`.
fn inject_request(attempt: u64) -> Vec<u8> {
    let body = json!({"event_id": "e1", "kind": "task_result", "prompt": "RESULT of e1.",
                      "lease": "0123abcd", "attempt": attempt});
    request("inject", body.to_string().as_bytes())
}

/// Runs `sql` on a connection of its own to the journal in `dir`.
fn alter(dir: &Scratch, sql: &str) {
    rusqlite::Connection::open(dir.path("journal.sqlite3"))
        .unwrap()
        .execute_batch(sql)
        .unwrap();
}

#[test]
fn a_delivery_record_that_cannot_be_read_or_written_answers_500_and_starts_nothing() {
    let dir = Scratch::new();
    let server = server(&dir, own_uid(), READ_TIMEOUT);
    let call = |raw: &[u8]| parse(&serve_pair(&server, raw).0);
    let set = json!({"session": OWNER, "route": "main"}).to_string();
    assert_eq!(call(&request("set_route", set.as_bytes())).status, 200);
    let _hold = hold_pair(&server);
    let internal = |response: c6_wire::Response| {
        let error: WireError = serde_json::from_slice(&response.body).unwrap();
        (response.status, error.code)
    };

    alter(
        &dir,
        "CREATE TRIGGER refuse BEFORE INSERT ON deliveries BEGIN SELECT RAISE(ABORT, 'refused'); END;",
    );
    assert_eq!(
        internal(call(&inject_request(1))),
        (500, ErrorCode::Internal),
        "a failed write"
    );
    alter(&dir, "DROP TRIGGER refuse; DROP TABLE deliveries;");
    assert_eq!(
        internal(call(&inject_request(1))),
        (500, ErrorCode::Internal),
        "a failed read"
    );

    let body = json!({"session": OWNER}).to_string();
    let owner: SessionStatus =
        serde_json::from_slice(&call(&request("status", body.as_bytes())).body).unwrap();
    assert_eq!(
        (owner.running, owner.queue_depth, owner.last_turn),
        (false, 0, None)
    );
    let idle: IdleStatus = serde_json::from_slice(&call(&request("is_idle", b"{}")).body).unwrap();
    assert!(idle.idle, "the held inbox_turns still waits");
}

#[test]
fn a_delivery_that_cannot_be_written_to_its_record_is_refused_and_stored_nowhere() {
    let dir = Scratch::new();
    let server = server(&dir, own_uid(), READ_TIMEOUT);
    let call = |method: &str, body: Value| {
        let (raw, _) = serve_pair(&server, &request(method, body.to_string().as_bytes()));
        parse(&raw).body
    };
    let report = |id: &str| {
        let status: SessionStatus =
            serde_json::from_slice(&call("status", json!({"session": OWNER, "turn": id}))).unwrap();
        status.turn.unwrap()
    };
    let deliver = |turn: &str| -> Ack {
        let body = json!({"session": OWNER, "turn": turn, "delivery": "sent"});
        serde_json::from_slice(&call("report_delivery", body)).unwrap()
    };
    call("set_route", json!({"session": OWNER, "route": "main"}));
    let hold = hold_pair(&server);
    let injected: InjectResponse =
        serde_json::from_slice(&parse(&serve_pair(&server, &inject_request(1)).0).body).unwrap();
    let turn = injected.turn.unwrap();
    assert_eq!(read_inbox(hold).turn.as_ref(), Some(&turn));
    until_ended(&turn, || report(&turn));
    let recorded = || {
        Journal::open(&dir.path("journal.sqlite3"))
            .unwrap()
            .delivery("e1", READ_TIMEOUT)
            .unwrap()
            .unwrap()
            .delivery
    };

    alter(
        &dir,
        "CREATE TRIGGER refuse BEFORE UPDATE ON deliveries BEGIN SELECT RAISE(ABORT, 'refused'); END;",
    );
    let refused = deliver(&turn);
    assert!(!refused.ok, "{refused:?}");
    assert_eq!(report(&turn).delivery, None);
    assert_eq!(recorded(), None);

    alter(&dir, "DROP TRIGGER refuse;");
    assert!(deliver(&turn).ok);
    assert_eq!(report(&turn).delivery, Some(Delivery::Sent));
    assert_eq!(recorded(), Some(InboxDeliveryDelivery::Sent));
}

#[test]
fn a_turn_ends_and_its_held_events_are_answered_while_an_inject_waits_for_the_journal_lock() {
    let dir = Scratch::new();
    let (decided, on_decided) = mpsc::sync_channel(1);
    let (release, on_release) = mpsc::sync_channel(1);
    let runner = Arc::new(Deciding {
        decided: Mutex::new(decided),
        release: Mutex::new(on_release),
        runs: Mutex::new(0),
    });
    let deadline = Duration::from_millis(1500);
    let server = server_with(&dir, runner, own_uid(), deadline);
    let call = |method: &str, body: Value| {
        let (raw, _) = serve_pair(&server, &request(method, body.to_string().as_bytes()));
        parse(&raw).body
    };
    call("set_route", json!({"session": OWNER, "route": "main"}));
    let _hold = hold_pair(&server);
    let answer: SubmitTurnResponse = serde_json::from_slice(&call(
        "submit_turn",
        json!({"session": "chat", "prompt": "hi"}),
    ))
    .unwrap();
    let turn = answer.turn.unwrap();
    on_decided.recv_timeout(Duration::from_secs(5)).unwrap();
    let (mut events, end) = UnixStream::pair().unwrap();
    events.write_all(&events_request("chat", &turn, 1)).unwrap();
    server.serve(&end);
    drop(end);

    // A reserved lock lets the record be read and makes its write wait.
    let lock = rusqlite::Connection::open(dir.path("journal.sqlite3")).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    let (paged, (raw, elapsed)) = std::thread::scope(|scope| {
        let reader = scope.spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            release.send(()).unwrap();
            let page = read_page(events);
            (page, Instant::now())
        });
        let injected = serve_pair(&server, &inject_request(1));
        let done = Instant::now();
        let (page, at) = reader.join().unwrap();
        assert!(at < done, "the page waited for the inject");
        (page, injected)
    });
    lock.execute_batch("ROLLBACK").unwrap();
    assert_eq!(parse(&raw).status, 500);
    assert!(elapsed >= deadline / 2, "{elapsed:?}");
    assert_eq!(
        paged.events.last().map(|e| e.r#type),
        Some(TurnEventType::End)
    );
    let owner: SessionStatus =
        serde_json::from_slice(&call("status", json!({"session": OWNER}))).unwrap();
    assert_eq!((owner.running, owner.last_turn), (false, None));
}
