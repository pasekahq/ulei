//! The session journal: its file and records, and turns on the scripted fake gateway whose
//! requests after a restart are the bytes of an engine that never stopped.

#![allow(missing_docs, clippy::unwrap_used)]

mod fake_gateway;

use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use fake_gateway::{
    Answer, FakeGateway, Scratch, call, gateway_error, request, route, route_info, text,
    text_block, text_step, tool_step, turns,
};
use serde_json::json;
use ulei_engine::agent::{Agent, Ended, Outcome, Step, Template, ThreadSleeper};
use ulei_engine::gateway::Client;
use ulei_engine::journal::{Error, Journal, Turn};
use ulei_engine::policy::Chain;
use ulei_engine::tools::{Interrupt, Registry};
use ulei_engine::wire::c4g::{
    Block, BlockType, Error as GatewayError, ErrorClass, Message, Role, StopReason,
};
use ulei_engine::wire::c8::Session;

const SLOW: Duration = Duration::from_secs(2);
const KEY: &str = "main";

fn user(text: &str) -> Message {
    Message {
        role: Role::User,
        content: vec![text_block(text)],
    }
}

fn assistant(text: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: vec![text_block(text)],
    }
}

fn block(value: serde_json::Value) -> Block {
    serde_json::from_value(value).unwrap()
}

fn step(request_id: &str) -> Step {
    Step {
        request_id: request_id.into(),
        route: Some(route_info()),
        usage: None,
    }
}

fn outcome(messages: Vec<Message>, steps: Vec<Step>) -> Outcome {
    Outcome {
        messages,
        steps,
        ended: Ended::Stopped(StopReason::End),
    }
}

/// Appends a turn of `prompt` whose model answered `reply` in one step.
fn append_text_turn(journal: &mut Journal, session: u64, prompt: &str, reply: &str) -> Turn {
    let turn = journal.next_turn(session).unwrap();
    let out = outcome(vec![assistant(reply)], vec![step(&turn.request_id(0))]);
    journal.append(turn, &[user(prompt)], &out).unwrap();
    turn
}

fn db(dir: &Scratch) -> PathBuf {
    dir.0.join("journal.sqlite3")
}

/// Runs the loop once after `history` with the request ids of `turn`.
fn run(
    client: &Client,
    cwd: &Path,
    history: &[Message],
    turn: Turn,
    interrupt: &Interrupt,
) -> Outcome {
    let tools = Registry::builtin(cwd);
    let mut chain = Chain::default();
    let template = Template {
        route: "chat".into(),
        system: Some("You are a test.".into()),
        max_tokens: 1024,
        effort: None,
        session_key: Some(KEY.into()),
    };
    let mut agent = Agent {
        client,
        tools: &tools,
        chain: &mut chain,
        cwd,
        template: &template,
    };
    agent.run(
        history,
        &|n| turn.request_id(n),
        interrupt,
        16,
        &mut ThreadSleeper,
    )
}

/// One engine process: it knows only its journal file and its gateway.
struct Engine<'a> {
    journal: Journal,
    client: Client,
    cwd: &'a Path,
}

impl<'a> Engine<'a> {
    fn start(db: &Path, fake: &FakeGateway, cwd: &'a Path) -> Self {
        Self {
            journal: Journal::open(db).unwrap(),
            client: Client::new(fake.socket(), SLOW, SLOW),
            cwd,
        }
    }

    /// One turn of the key's current session from its journaled history, then journaled.
    fn turn(&mut self, prompt: &str, interrupt: &Interrupt) -> Outcome {
        let session = self.journal.session(KEY).unwrap();
        let turn = self.journal.next_turn(session.id).unwrap();
        let mut history = self.journal.messages(session.id).unwrap();
        history.push(user(prompt));
        let out = run(&self.client, self.cwd, &history, turn, interrupt);
        self.journal.append(turn, &[user(prompt)], &out).unwrap();
        out
    }
}

/// Every `tool_use` of `messages` is answered by a `tool_result` in the next message.
fn tool_uses_answered(messages: &[Message]) -> bool {
    messages.iter().enumerate().all(|(i, m)| {
        m.content
            .iter()
            .filter(|b| b.r#type == BlockType::ToolUse)
            .all(|u| {
                messages.get(i + 1).is_some_and(|next| {
                    next.content
                        .iter()
                        .any(|b| b.r#type == BlockType::ToolResult && b.id == u.id)
                })
            })
    })
}

#[test]
fn a_new_file_reopens_with_its_session_messages_and_steps() {
    let dir = Scratch::new();
    let path = db(&dir);
    let mut journal = Journal::open(&path).unwrap();
    let session = journal.session(KEY).unwrap();
    assert_eq!(
        session,
        Session {
            id: 1,
            key: KEY.into()
        }
    );
    assert_eq!(journal.messages(1).unwrap(), []);
    assert_eq!(
        journal.next_turn(1).unwrap(),
        Turn {
            session: 1,
            number: 1
        }
    );
    append_text_turn(&mut journal, 1, "Hello.", "Hi.");
    drop(journal);

    let mut journal = Journal::open(&path).unwrap();
    assert_eq!(journal.session(KEY).unwrap(), session);
    assert_eq!(
        journal.messages(1).unwrap(),
        [user("Hello."), assistant("Hi.")]
    );
    let steps = journal.steps(1).unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!((steps[0].session, steps[0].turn, steps[0].step), (1, 1, 0));
    assert_eq!(steps[0].request_id, "s1-t1-0");
    assert_eq!(
        steps[0].route,
        Some(serde_json::from_value(serde_json::to_value(route_info()).unwrap()).unwrap())
    );
    assert_eq!(steps[0].usage, None);
    assert_eq!(journal.next_turn(1).unwrap().number, 2);
    assert!(matches!(journal.messages(2), Err(Error::UnknownSession(2))));
}

#[test]
fn appends_keep_message_and_step_order_across_turns() {
    let dir = Scratch::new();
    let mut journal = Journal::open(&db(&dir)).unwrap();
    let id = journal.session(KEY).unwrap().id;
    let turn = journal.next_turn(id).unwrap();
    let first = outcome(
        vec![assistant("one"), user("nudge"), assistant("two")],
        vec![step(&turn.request_id(0)), step(&turn.request_id(1))],
    );
    journal.append(turn, &[user("go")], &first).unwrap();
    append_text_turn(&mut journal, id, "again", "three");

    assert_eq!(
        journal.messages(id).unwrap(),
        [
            user("go"),
            assistant("one"),
            user("nudge"),
            assistant("two"),
            user("again"),
            assistant("three"),
        ]
    );
    let steps: Vec<(u64, u64, String)> = journal
        .steps(id)
        .unwrap()
        .into_iter()
        .map(|s| (s.turn, s.step, s.request_id))
        .collect();
    assert_eq!(
        steps,
        [
            (1, 0, "s1-t1-0".into()),
            (1, 1, "s1-t1-1".into()),
            (2, 0, "s1-t2-0".into()),
        ]
    );
}

#[test]
fn stored_messages_round_trip_to_the_same_values_and_bytes() {
    let dir = Scratch::new();
    let path = db(&dir);
    let messages = vec![
        Message {
            role: Role::Assistant,
            content: vec![
                block(json!({"type": "thinking", "vendor": "vendor-a", "opaque": "sig\n1"})),
                block(json!({"type": "text", "text": "Ünïcode \u{2028} \"quoted\" / \t"})),
                block(json!({
                    "type": "tool_use", "id": "c1", "name": "write_file",
                    "input": {"z": 1, "a": {"y": [1, 2.5, -0.0, 1e300], "b": null}, "m": true}
                })),
            ],
        },
        Message {
            role: Role::User,
            content: vec![
                block(
                    json!({"type": "tool_result", "id": "c1", "content": "ok", "is_error": false}),
                ),
                block(json!({"type": "image", "media_type": "image/png", "base64": "iVBORw0K"})),
            ],
        },
    ];
    let mut journal = Journal::open(&path).unwrap();
    let id = journal.session(KEY).unwrap().id;
    let turn = journal.next_turn(id).unwrap();
    journal
        .append(turn, &[user("Write.")], &outcome(messages.clone(), vec![]))
        .unwrap();
    drop(journal);

    let loaded = Journal::open(&path).unwrap().messages(id).unwrap();
    let mut want = vec![user("Write.")];
    want.extend(messages);
    assert_eq!(loaded, want);
    assert_eq!(
        serde_json::to_vec(&loaded).unwrap(),
        serde_json::to_vec(&want).unwrap()
    );
}

#[test]
fn a_failed_append_leaves_the_previous_state() {
    let dir = Scratch::new();
    let path = db(&dir);
    let mut journal = Journal::open(&path).unwrap();
    let id = journal.session(KEY).unwrap().id;
    append_text_turn(&mut journal, id, "first", "one");
    let turn = journal.next_turn(id).unwrap();
    // The second step reuses a request id of turn 1, so its insert fails after the messages.
    let failing = outcome(
        vec![assistant("two")],
        vec![step(&turn.request_id(0)), step("s1-t1-0")],
    );
    assert!(matches!(
        journal.append(turn, &[user("second")], &failing),
        Err(Error::Sqlite(_))
    ));
    drop(journal);

    let mut journal = Journal::open(&path).unwrap();
    assert_eq!(
        journal.messages(id).unwrap(),
        [user("first"), assistant("one")]
    );
    assert_eq!(journal.steps(id).unwrap().len(), 1);
    assert_eq!(journal.next_turn(id).unwrap(), turn);
    append_text_turn(&mut journal, id, "second", "two");
    assert_eq!(journal.messages(id).unwrap().len(), 4);
}

#[test]
fn an_append_of_a_turn_that_is_not_the_next_is_refused() {
    let dir = Scratch::new();
    let mut journal = Journal::open(&db(&dir)).unwrap();
    let id = journal.session(KEY).unwrap().id;
    let turn = append_text_turn(&mut journal, id, "first", "one");
    assert!(matches!(
        journal.append(turn, &[user("again")], &outcome(vec![], vec![])),
        Err(Error::StaleTurn { turn: 1, next: 2 })
    ));
    assert_eq!(journal.messages(id).unwrap().len(), 2);
}

#[test]
fn a_file_of_another_schema_version_is_refused() {
    let dir = Scratch::new();
    let path = db(&dir);
    drop(Journal::open(&path).unwrap());
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("PRAGMA user_version=2")
        .unwrap();

    assert!(matches!(Journal::open(&path), Err(Error::SchemaVersion(2))));
    let version: i64 = rusqlite::Connection::open(&path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 2);
}

#[test]
fn new_session_becomes_current_and_the_old_one_stays_readable() {
    let dir = Scratch::new();
    let mut journal = Journal::open(&db(&dir)).unwrap();
    let old = journal.session(KEY).unwrap();
    append_text_turn(&mut journal, old.id, "first", "one");
    let other = journal.session("other").unwrap();

    let new = journal.new_session(KEY).unwrap();
    assert_eq!(journal.session(KEY).unwrap(), new);
    assert_ne!(new.id, old.id);
    assert_eq!(journal.messages(new.id).unwrap(), []);
    assert_eq!(journal.next_turn(new.id).unwrap().number, 1);
    assert_eq!(
        journal.messages(old.id).unwrap(),
        [user("first"), assistant("one")]
    );
    assert_eq!(journal.session("other").unwrap(), other);
    assert!(matches!(journal.session(""), Err(Error::InvalidKey)));
    assert!(matches!(
        journal.new_session(&"k".repeat(129)),
        Err(Error::InvalidKey)
    ));
}

#[test]
fn turn_two_after_a_restart_sends_the_bytes_of_an_engine_that_never_stopped() {
    let dir = Scratch::new();
    let cwd = Scratch::new();
    let turn_one = || {
        vec![
            tool_step(vec![call(
                "c1",
                "write_file",
                json!({"path": "notes.txt", "content": "alpha\n"}),
            )]),
            text_step("Wrote it."),
        ]
    };
    let turn_two = || {
        vec![
            tool_step(vec![call("c2", "read_file", json!({"path": "notes.txt"}))]),
            text_step("It says alpha."),
        ]
    };

    let restarted = {
        let path = db(&dir);
        let fake = FakeGateway::start(turn_one());
        let out = Engine::start(&path, &fake, &cwd.0).turn("First.", &Interrupt::new());
        assert!(
            matches!(out.ended, Ended::Stopped(StopReason::End)),
            "{out:?}"
        );
        assert_eq!(out.steps.len(), 2);
        drop(fake);

        let fake = FakeGateway::start(turn_two());
        let out = Engine::start(&path, &fake, &cwd.0).turn("Second.", &Interrupt::new());
        assert!(
            matches!(out.ended, Ended::Stopped(StopReason::End)),
            "{out:?}"
        );
        turns(&fake)
    };

    let control = {
        let mut script = turn_one();
        script.extend(turn_two());
        let fake = FakeGateway::start(script);
        let client = Client::new(fake.socket(), SLOW, SLOW);
        let mut journal = Journal::open(&dir.0.join("control.sqlite3")).unwrap();
        let session = journal.session(KEY).unwrap().id;
        let interrupt = Interrupt::new();
        let mut history = vec![user("First.")];
        let turn = journal.next_turn(session).unwrap();
        let out = run(&client, &cwd.0, &history, turn, &interrupt);
        journal.append(turn, &[user("First.")], &out).unwrap();
        history.extend(out.messages);
        history.push(user("Second."));
        let turn = journal.next_turn(session).unwrap();
        run(&client, &cwd.0, &history, turn, &interrupt);
        turns(&fake).split_off(2)
    };

    assert_eq!(restarted.len(), 2);
    assert_eq!(restarted, control);
    let first = request(&restarted[0]);
    assert_eq!(first.request_id, "s1-t2-0");
    assert_eq!(first.messages.len(), 5);
    assert!(tool_uses_answered(&first.messages));
}

#[test]
fn after_new_the_next_request_carries_no_earlier_message() {
    let dir = Scratch::new();
    let path = db(&dir);
    {
        let fake = FakeGateway::start(vec![text_step("Hi.")]);
        let mut engine = Engine::start(&path, &fake, &dir.0);
        engine.turn("First.", &Interrupt::new());
        engine.journal.new_session(KEY).unwrap();
    }

    let fake = FakeGateway::start(vec![text_step("Fresh.")]);
    let mut engine = Engine::start(&path, &fake, &dir.0);
    engine.turn("Again.", &Interrupt::new());

    let sent = turns(&fake);
    assert_eq!(sent.len(), 1);
    let body = request(&sent[0]);
    assert_eq!(body.messages, [user("Again.")]);
    assert_eq!(body.request_id, "s2-t1-0");
    assert_eq!(
        engine.journal.messages(1).unwrap(),
        [user("First."), assistant("Hi.")]
    );
}

#[test]
fn an_interrupted_turn_journals_a_history_the_next_turn_sends() {
    let dir = Scratch::new();
    let path = db(&dir);
    let first = {
        let fake = FakeGateway::start(vec![
            tool_step(vec![call(
                "c1",
                "search_files",
                json!({"pattern": "*", "target": "files"}),
            )]),
            Answer::Hang(vec![route(), text("partial")]),
        ]);
        let interrupt = Interrupt::new();
        let out = thread::scope(|scope| {
            let remote = interrupt.clone();
            let fake = &fake;
            scope.spawn(move || {
                let deadline = Instant::now() + SLOW;
                while turns(fake).len() < 2 && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(5));
                }
                remote.interrupt();
            });
            Engine::start(&path, fake, &dir.0).turn("First.", &interrupt)
        });
        assert!(matches!(out.ended, Ended::Interrupted), "{out:?}");
        assert_eq!(out.messages.len(), 2);
        out.messages
    };

    let fake = FakeGateway::start(vec![text_step("Back.")]);
    let out = Engine::start(&path, &fake, &dir.0).turn("Again.", &Interrupt::new());
    assert!(
        matches!(out.ended, Ended::Stopped(StopReason::End)),
        "{out:?}"
    );

    let sent = turns(&fake);
    assert_eq!(sent.len(), 1);
    let body = request(&sent[0]);
    let mut want = vec![user("First.")];
    want.extend(first);
    want.push(user("Again."));
    assert_eq!(body.messages, want);
    assert!(tool_uses_answered(&body.messages));
    assert_eq!(body.request_id, "s1-t2-0");
}

#[test]
fn a_quota_exhausted_turn_journals_a_history_the_next_turn_sends() {
    let dir = Scratch::new();
    let path = db(&dir);
    {
        let quota = GatewayError {
            resets_at: Some(1_794_924_000),
            ..gateway_error(ErrorClass::QuotaExhausted)
        };
        let fake = FakeGateway::start(vec![
            tool_step(vec![call(
                "c1",
                "search_files",
                json!({"pattern": "*", "target": "files"}),
            )]),
            Answer::Reject(429, quota),
        ]);
        let out = Engine::start(&path, &fake, &dir.0).turn("First.", &Interrupt::new());
        assert!(matches!(out.ended, Ended::QuotaExhausted { .. }), "{out:?}");
        assert_eq!(out.steps.len(), 2);
    }

    let fake = FakeGateway::start(vec![text_step("Back.")]);
    let mut engine = Engine::start(&path, &fake, &dir.0);
    let out = engine.turn("Again.", &Interrupt::new());
    assert!(
        matches!(out.ended, Ended::Stopped(StopReason::End)),
        "{out:?}"
    );

    let sent = turns(&fake);
    assert_eq!(sent.len(), 1);
    let body = request(&sent[0]);
    let roles: Vec<Role> = body.messages.iter().map(|m| m.role).collect();
    assert_eq!(roles, [Role::User, Role::Assistant, Role::User, Role::User]);
    assert_eq!(body.messages[0], user("First."));
    assert_eq!(body.messages[3], user("Again."));
    assert!(tool_uses_answered(&body.messages));
    assert_eq!(body.request_id, "s1-t2-0");
    assert_eq!(engine.journal.messages(1).unwrap().len(), 5);
}
