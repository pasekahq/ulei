//! The session journal: its file and records, and turns on the scripted fake gateway whose
//! requests after a restart are the bytes of an engine that never stopped.

#![allow(missing_docs, clippy::unwrap_used)]

mod fake_gateway;

use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use fake_gateway::{
    Answer, FakeGateway, Recorder, SUMMARY_REQUEST, Scratch, call, empty_step, gateway_error,
    request, route, route_info, search_step, text, text_block, text_step, thinking, tool_step,
    turns, untagged, untagged_bodies, usage,
};
use serde_json::json;
use ulei_engine::agent::{Agent, DEFAULT_CONCURRENT_TOOL_TIMEOUT, Ended, Outcome, Step, Template};
use ulei_engine::gateway::Client;
use ulei_engine::journal::{Error, Journal, SCHEMA_VERSION, Turn};
use ulei_engine::policy::{Chain, Origin};
use ulei_engine::tools::redact::Redactor;
use ulei_engine::tools::{Interrupt, Registry};
use ulei_engine::wire::c4g::{
    Block, BlockType, Error as GatewayError, ErrorClass, Message, Role, StopReason,
};
use ulei_engine::wire::c8::{InboxDelivery, InboxDeliveryDelivery, Session};

const SLOW: Duration = Duration::from_secs(2);
const KEY: &str = "main";
const STEPS: usize = 16;

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
        compacted: None,
    }
}

/// Appends a turn of `prompt` whose model answered `reply` in one step.
fn append_text_turn(journal: &mut Journal, session: u64, prompt: &str, reply: &str) -> Turn {
    let turn = journal.next_turn(session).unwrap();
    let ids = turn.request_ids().unwrap();
    let out = outcome(vec![assistant(reply)], vec![step(&ids.step(0))]);
    journal.append(turn, &[user(prompt)], &out).unwrap();
    turn
}

fn db(dir: &Scratch) -> PathBuf {
    dir.0.join("journal.sqlite3")
}

/// Runs the loop once after `history` with the request ids of `turn`; waits are only recorded.
fn run(
    client: &Client,
    cwd: &Path,
    history: &[Message],
    turn: Turn,
    interrupt: &Interrupt,
    max_steps: usize,
) -> Outcome {
    let ids = turn.request_ids().unwrap();
    let tools = Registry::builtin(
        cwd,
        Redactor { enabled: true },
        None,
        &fake_gateway::skills(cwd),
        None,
    );
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
        batch_timeout: Some(DEFAULT_CONCURRENT_TOOL_TIMEOUT),
        empty_guard: true,
        origin: Origin::Other,
        compaction: None,
        context: None,
        interim: None,
    };
    agent.run(
        history,
        &|n| ids.step(n),
        interrupt,
        max_steps,
        &mut Recorder::default(),
    )
}

/// One engine process: it knows only its journal file and its gateway.
struct Engine<'a> {
    journal: Journal,
    client: Client,
    cwd: &'a Path,
    max_steps: usize,
}

impl<'a> Engine<'a> {
    fn start(db: &Path, fake: &FakeGateway, cwd: &'a Path) -> Self {
        Self {
            journal: Journal::open(db).unwrap(),
            client: Client::new(fake.socket(), SLOW, SLOW),
            cwd,
            max_steps: STEPS,
        }
    }

    /// One turn of the key's current session from its journaled history, then journaled.
    fn turn(&mut self, prompt: &str, interrupt: &Interrupt) -> Outcome {
        let session = self.journal.session(KEY).unwrap();
        let turn = self.journal.next_turn(session.id).unwrap();
        let mut history = self.journal.messages(session.id).unwrap();
        history.push(user(prompt));
        let out = run(
            &self.client,
            self.cwd,
            &history,
            turn,
            interrupt,
            self.max_steps,
        );
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
            key: KEY.into(),
            system_prompt: None,
            route: None
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
    assert_eq!(untagged(&steps[0].request_id), "s1-t1-0");
    assert_eq!(
        steps[0].route,
        Some(serde_json::from_value(serde_json::to_value(route_info()).unwrap()).unwrap())
    );
    assert_eq!(steps[0].usage, None);
    assert_eq!(journal.next_turn(1).unwrap().number, 2);
    assert!(matches!(journal.messages(2), Err(Error::UnknownSession(2))));
}

#[test]
fn equal_turns_of_two_journals_and_a_rerun_send_different_request_ids() {
    let dir = Scratch::new();
    let mut a = Journal::open(&dir.path("a.sqlite3")).unwrap();
    let mut b = Journal::open(&dir.path("b.sqlite3")).unwrap();
    let (session_a, session_b) = (a.session(KEY).unwrap().id, b.session(KEY).unwrap().id);
    let turn_a = a.next_turn(session_a).unwrap();
    let turn_b = b.next_turn(session_b).unwrap();
    assert_eq!(turn_a, turn_b);
    // A run killed before its append leaves the journal as it was, so its rerun takes the turn.
    let rerun = a.next_turn(turn_a.session).unwrap();
    assert_eq!(rerun, turn_a);

    let ids: Vec<String> = [turn_a, turn_b, rerun]
        .iter()
        .map(|turn| turn.request_ids().unwrap().step(0))
        .collect();
    for id in &ids {
        assert_eq!(untagged(id), "s1-t1-0");
    }
    assert_ne!(ids[0], ids[1]);
    assert_ne!(ids[0], ids[2]);
    assert_ne!(ids[1], ids[2]);
}

#[test]
fn the_steps_of_one_run_send_distinct_request_ids_within_64_bytes() {
    let largest = i64::MAX.cast_unsigned();
    let ids = Turn {
        session: largest,
        number: largest,
    }
    .request_ids()
    .unwrap();
    let steps = [0, 1, 2, 999_999_999].map(|n| ids.step(n));
    for (i, id) in steps.iter().enumerate() {
        assert!(id.len() <= 64, "{id}");
        assert!(steps[..i].iter().all(|other| other != id), "{steps:?}");
    }
    assert_eq!(
        untagged(&steps[3]),
        format!("s{largest}-t{largest}-999999999")
    );
}

#[test]
fn appends_keep_message_and_step_order_across_turns() {
    let dir = Scratch::new();
    let mut journal = Journal::open(&db(&dir)).unwrap();
    let id = journal.session(KEY).unwrap().id;
    let turn = journal.next_turn(id).unwrap();
    let ids = turn.request_ids().unwrap();
    let first = outcome(
        vec![assistant("one"), user("nudge"), assistant("two")],
        vec![step(&ids.step(0)), step(&ids.step(1))],
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
        .map(|s| (s.turn, s.step, untagged(&s.request_id).to_owned()))
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
    let reused = journal.steps(id).unwrap().remove(0).request_id;
    let turn = journal.next_turn(id).unwrap();
    // The second step reuses a request id of turn 1, so its insert fails after the messages.
    let failing = outcome(
        vec![assistant("two")],
        vec![step(&turn.request_ids().unwrap().step(0)), step(&reused)],
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
    let newer = SCHEMA_VERSION + 1;
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(&format!("PRAGMA user_version={newer}"))
        .unwrap();

    assert!(matches!(Journal::open(&path), Err(Error::SchemaVersion(v)) if v == newer));
    let version: i64 = rusqlite::Connection::open(&path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, newer);
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
        let out = run(&client, &cwd.0, &history, turn, &interrupt, STEPS);
        journal.append(turn, &[user("First.")], &out).unwrap();
        history.extend(out.messages);
        history.push(user("Second."));
        let turn = journal.next_turn(session).unwrap();
        run(&client, &cwd.0, &history, turn, &interrupt, STEPS);
        turns(&fake).split_off(2)
    };

    assert_eq!(restarted.len(), 2);
    assert_eq!(untagged_bodies(&restarted), untagged_bodies(&control));
    let first = request(&restarted[0]);
    assert_eq!(untagged(&first.request_id), "s1-t2-0");
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
    assert_eq!(untagged(&body.request_id), "s2-t1-0");
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
            search_step("c1"),
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
    assert_eq!(untagged(&body.request_id), "s1-t2-0");
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
        let fake = FakeGateway::start(vec![search_step("c1"), Answer::Reject(429, quota)]);
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
    assert_eq!(untagged(&body.request_id), "s1-t2-0");
    assert_eq!(engine.journal.messages(1).unwrap().len(), 5);
}

#[test]
fn a_summary_turn_and_an_empty_turn_replay_their_history_after_a_restart() {
    let dir = Scratch::new();
    let cwd = Scratch::new();
    let path = db(&dir);
    let summary_turn = || vec![search_step("c1"), text_step("Summary.")];
    let empty_turn = || {
        vec![
            search_step("c2"),
            empty_step(vec![thinking("sig-1")]),
            empty_step(vec![usage(0)]),
            empty_step(vec![usage(0)]),
        ]
    };

    let restarted = {
        let fake = FakeGateway::start(summary_turn());
        let mut engine = Engine::start(&path, &fake, &cwd.0);
        engine.max_steps = 1;
        let out = engine.turn("First.", &Interrupt::new());
        assert!(matches!(out.ended, Ended::MaxSteps), "{out:?}");
        drop(fake);

        let fake = FakeGateway::start(empty_turn());
        let out = Engine::start(&path, &fake, &cwd.0).turn("Second.", &Interrupt::new());
        assert!(
            matches!(out.ended, Ended::Stopped(StopReason::End)),
            "{out:?}"
        );
        assert_eq!(turns(&fake).len(), 4);
        drop(fake);

        let fake = FakeGateway::start(vec![text_step("Back.")]);
        Engine::start(&path, &fake, &cwd.0).turn("Third.", &Interrupt::new());
        turns(&fake)
    };

    let control = {
        let mut script = summary_turn();
        script.extend(empty_turn());
        script.push(text_step("Back."));
        let fake = FakeGateway::start(script);
        let client = Client::new(fake.socket(), SLOW, SLOW);
        let mut journal = Journal::open(&dir.0.join("control.sqlite3")).unwrap();
        let session = journal.session(KEY).unwrap().id;
        let interrupt = Interrupt::new();
        let mut history = Vec::new();
        for (prompt, max_steps) in [("First.", 1), ("Second.", STEPS), ("Third.", STEPS)] {
            history.push(user(prompt));
            let turn = journal.next_turn(session).unwrap();
            let out = run(&client, &cwd.0, &history, turn, &interrupt, max_steps);
            journal.append(turn, &[user(prompt)], &out).unwrap();
            history.extend(out.messages);
        }
        turns(&fake).split_off(6)
    };

    assert_eq!(restarted.len(), 1);
    assert_eq!(untagged_bodies(&restarted), untagged_bodies(&control));
    let body = request(&restarted[0]);
    let shapes: Vec<(Role, String)> = body
        .messages
        .iter()
        .map(|m| {
            let kinds: Vec<&str> = m.content.iter().map(|b| b.r#type.as_str()).collect();
            let text: String = m.content.iter().filter_map(|b| b.text.clone()).collect();
            (m.role, format!("{}:{text}", kinds.join(",")))
        })
        .collect();
    assert_eq!(
        shapes,
        [
            (Role::User, "text:First.".to_owned()),
            (Role::Assistant, "tool_use:".to_owned()),
            (Role::User, "tool_result:".to_owned()),
            (Role::User, format!("text:{SUMMARY_REQUEST}")),
            (Role::Assistant, "text:Summary.".to_owned()),
            (Role::User, "text:Second.".to_owned()),
            (Role::Assistant, "tool_use:".to_owned()),
            (Role::User, "tool_result:".to_owned()),
            (Role::Assistant, "text:(empty)".to_owned()),
            (Role::User, "text:Third.".to_owned()),
        ]
    );
    assert!(tool_uses_answered(&body.messages));
    assert_eq!(untagged(&body.request_id), "s1-t3-0");
}

/// `(seq, turn, active, compacted, message)` of every stored message of `session`.
fn records(journal: &Journal, session: u64) -> Vec<(u64, u64, bool, bool, Message)> {
    journal
        .records(session)
        .unwrap()
        .into_iter()
        .map(|r| {
            (
                r.seq,
                r.turn,
                r.active.unwrap(),
                r.compacted.unwrap(),
                serde_json::from_value(serde_json::Value::Object(r.message)).unwrap(),
            )
        })
        .collect()
}

#[test]
fn a_compaction_archives_the_live_set_and_a_turn_loads_only_the_compacted_set_in_order() {
    let dir = Scratch::new();
    let path = db(&dir);
    let mut journal = Journal::open(&path).unwrap();
    let id = journal.session(KEY).unwrap().id;
    append_text_turn(&mut journal, id, "first", "one");
    append_text_turn(&mut journal, id, "second", "two");
    let summary = [user("Summary: first and second."), assistant("Noted.")];

    journal.compact(id, &summary).unwrap();
    assert_eq!(journal.messages(id).unwrap(), summary);
    assert_eq!(journal.next_turn(id).unwrap().number, 3);
    append_text_turn(&mut journal, id, "third", "three");
    drop(journal);

    let mut journal = Journal::open(&path).unwrap();
    assert_eq!(
        journal.messages(id).unwrap(),
        [
            summary[0].clone(),
            summary[1].clone(),
            user("third"),
            assistant("three")
        ]
    );
    assert_eq!(
        records(&journal, id),
        [
            (0, 1, false, true, user("first")),
            (1, 1, false, true, assistant("one")),
            (2, 2, false, true, user("second")),
            (3, 2, false, true, assistant("two")),
            (4, 2, true, false, summary[0].clone()),
            (5, 2, true, false, summary[1].clone()),
            (6, 3, true, false, user("third")),
            (7, 3, true, false, assistant("three")),
        ]
    );

    journal.compact(id, &[user("Summary: all three.")]).unwrap();
    assert_eq!(journal.messages(id).unwrap(), [user("Summary: all three.")]);
    let flags: Vec<(u64, u64, bool, bool)> = records(&journal, id)
        .into_iter()
        .map(|(seq, turn, active, compacted, _)| (seq, turn, active, compacted))
        .collect();
    assert_eq!(flags.len(), 9);
    assert!(flags[..8].iter().all(|f| !f.2 && f.3));
    assert_eq!(flags[8], (8, 3, true, false));
    assert_eq!(journal.next_turn(id).unwrap().number, 4);
}

#[test]
fn a_failed_compaction_leaves_the_session_unchanged() {
    let dir = Scratch::new();
    let path = db(&dir);
    let mut journal = Journal::open(&path).unwrap();
    let id = journal.session(KEY).unwrap().id;
    append_text_turn(&mut journal, id, "first", "one");
    let before = records(&journal, id);
    drop(journal);
    // The second message of the compacted set fails its insert after the archive update ran.
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER refuse BEFORE INSERT ON messages WHEN NEW.message LIKE '%refused%'
BEGIN SELECT RAISE(ABORT, 'refused'); END;",
        )
        .unwrap();

    let mut journal = Journal::open(&path).unwrap();
    assert!(matches!(
        journal.compact(id, &[user("Summary."), user("refused")]),
        Err(Error::Sqlite(_))
    ));
    drop(journal);

    let mut journal = Journal::open(&path).unwrap();
    assert_eq!(records(&journal, id), before);
    assert_eq!(
        journal.messages(id).unwrap(),
        [user("first"), assistant("one")]
    );
    let empty = journal.new_session("other").unwrap().id;
    assert!(matches!(
        journal.compact(empty, &[user("Summary.")]),
        Err(Error::NoTurn(e)) if e == empty
    ));
    assert_eq!(journal.records(empty).unwrap(), []);
    assert!(matches!(
        journal.compact(99, &[user("Summary.")]),
        Err(Error::UnknownSession(99))
    ));
}

#[test]
fn a_compaction_to_an_empty_set_is_refused_and_the_session_stays_unchanged() {
    let dir = Scratch::new();
    let path = db(&dir);
    let mut journal = Journal::open(&path).unwrap();
    let id = journal.session(KEY).unwrap().id;
    append_text_turn(&mut journal, id, "first", "one");
    let before = records(&journal, id);

    assert!(matches!(
        journal.compact(id, &[]),
        Err(Error::EmptyCompaction(e)) if e == id
    ));
    drop(journal);

    let mut journal = Journal::open(&path).unwrap();
    assert_eq!(records(&journal, id), before);
    assert_eq!(
        journal.messages(id).unwrap(),
        [user("first"), assistant("one")]
    );
    assert_eq!(journal.next_turn(id).unwrap().number, 2);
    journal.compact(id, &[user("Summary.")]).unwrap();
    assert_eq!(journal.messages(id).unwrap(), [user("Summary.")]);
}

#[test]
fn a_version_2_file_opens_with_every_message_live() {
    let dir = Scratch::new();
    let path = db(&dir);
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(
            "CREATE TABLE sessions (id INTEGER PRIMARY KEY, key TEXT NOT NULL, system_prompt TEXT);
CREATE INDEX sessions_key ON sessions (key, id);
CREATE TABLE messages (
  session INTEGER NOT NULL REFERENCES sessions (id), seq INTEGER NOT NULL,
  turn INTEGER NOT NULL, message TEXT NOT NULL, PRIMARY KEY (session, seq)) WITHOUT ROWID;
CREATE TABLE steps (
  session INTEGER NOT NULL REFERENCES sessions (id), turn INTEGER NOT NULL,
  step INTEGER NOT NULL, request_id TEXT NOT NULL UNIQUE, route TEXT, usage TEXT,
  PRIMARY KEY (session, turn, step)) WITHOUT ROWID;
INSERT INTO sessions (key, system_prompt) VALUES ('main', 'You are a test.');
INSERT INTO messages VALUES (1, 0, 1, '{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"earlier\"}]}');
INSERT INTO messages VALUES (1, 1, 1, '{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"noted\"}]}');
INSERT INTO steps VALUES (1, 1, 0, 's1-t1-0', NULL, NULL);
PRAGMA user_version=2;",
        )
        .unwrap();

    let mut journal = Journal::open(&path).unwrap();
    assert_eq!(
        journal.session(KEY).unwrap().system_prompt.as_deref(),
        Some("You are a test.")
    );
    assert_eq!(
        journal.messages(1).unwrap(),
        [user("earlier"), assistant("noted")]
    );
    assert_eq!(
        records(&journal, 1),
        [
            (0, 1, true, false, user("earlier")),
            (1, 1, true, false, assistant("noted")),
        ]
    );
    journal.compact(1, &[user("Summary.")]).unwrap();
    assert_eq!(journal.messages(1).unwrap(), [user("Summary.")]);
    drop(journal);
    let version: i64 = rusqlite::Connection::open(&path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
}

#[test]
fn a_version_3_file_opens_with_its_rows_and_no_route_and_stores_one() {
    let dir = Scratch::new();
    let path = db(&dir);
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(
            "CREATE TABLE sessions (id INTEGER PRIMARY KEY, key TEXT NOT NULL, system_prompt TEXT);
CREATE INDEX sessions_key ON sessions (key, id);
CREATE TABLE messages (
  session INTEGER NOT NULL REFERENCES sessions (id), seq INTEGER NOT NULL,
  turn INTEGER NOT NULL, message TEXT NOT NULL, active INTEGER NOT NULL DEFAULT 1,
  compacted INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (session, seq)) WITHOUT ROWID;
CREATE INDEX messages_live ON messages (session, active, seq);
CREATE TABLE steps (
  session INTEGER NOT NULL REFERENCES sessions (id), turn INTEGER NOT NULL,
  step INTEGER NOT NULL, request_id TEXT NOT NULL UNIQUE, route TEXT, usage TEXT,
  PRIMARY KEY (session, turn, step)) WITHOUT ROWID;
INSERT INTO sessions (key, system_prompt) VALUES ('main', 'You are a test.');
INSERT INTO messages VALUES (1, 0, 1, '{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"earlier\"}]}', 0, 1);
INSERT INTO messages VALUES (1, 1, 1, '{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"Summary.\"}]}', 1, 0);
INSERT INTO steps VALUES (1, 1, 0, 's1-t1-0', NULL, NULL);
PRAGMA user_version=3;",
        )
        .unwrap();

    let mut journal = Journal::open(&path).unwrap();
    assert_eq!(
        journal.session(KEY).unwrap(),
        Session {
            id: 1,
            key: KEY.to_owned(),
            system_prompt: Some("You are a test.".to_owned()),
            route: None,
        }
    );
    assert_eq!(
        records(&journal, 1),
        [
            (0, 1, false, true, user("earlier")),
            (1, 1, true, false, user("Summary.")),
        ]
    );
    assert_eq!(journal.steps(1).unwrap().len(), 1);
    assert_eq!(journal.next_turn(1).unwrap().number, 2);
    journal.store_route(KEY, "deep", SLOW).unwrap();
    drop(journal);
    let journal = Journal::open(&path).unwrap();
    assert_eq!(journal.route(KEY, SLOW).unwrap().as_deref(), Some("deep"));
    let version: i64 = rusqlite::Connection::open(&path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
}

/// The record of inbox event `event_id` at `attempt` naming `turn`, without a delivery.
fn accepted(event_id: &str, attempt: u64, turn: &str) -> InboxDelivery {
    InboxDelivery {
        event_id: event_id.to_owned(),
        attempt,
        turn: turn.to_owned(),
        delivery: None,
    }
}

#[test]
fn a_version_4_file_opens_with_its_rows_and_no_delivery_record_and_stores_one() {
    let dir = Scratch::new();
    let path = db(&dir);
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch(
            "CREATE TABLE sessions (
  id INTEGER PRIMARY KEY, key TEXT NOT NULL, system_prompt TEXT, route TEXT);
CREATE INDEX sessions_key ON sessions (key, id);
CREATE TABLE messages (
  session INTEGER NOT NULL REFERENCES sessions (id), seq INTEGER NOT NULL,
  turn INTEGER NOT NULL, message TEXT NOT NULL, active INTEGER NOT NULL DEFAULT 1,
  compacted INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (session, seq)) WITHOUT ROWID;
CREATE INDEX messages_live ON messages (session, active, seq);
CREATE TABLE steps (
  session INTEGER NOT NULL REFERENCES sessions (id), turn INTEGER NOT NULL,
  step INTEGER NOT NULL, request_id TEXT NOT NULL UNIQUE, route TEXT, usage TEXT,
  PRIMARY KEY (session, turn, step)) WITHOUT ROWID;
INSERT INTO sessions (key, system_prompt, route) VALUES ('main', 'You are a test.', 'deep');
INSERT INTO messages VALUES (1, 0, 1, '{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"earlier\"}]}', 1, 0);
INSERT INTO steps VALUES (1, 1, 0, 's1-t1-0', NULL, NULL);
PRAGMA user_version=4;",
        )
        .unwrap();

    let mut journal = Journal::open(&path).unwrap();
    assert_eq!(journal.route(KEY, SLOW).unwrap().as_deref(), Some("deep"));
    assert_eq!(records(&journal, 1), [(0, 1, true, false, user("earlier"))]);
    assert_eq!(journal.next_turn(1).unwrap().number, 2);
    assert_eq!(journal.delivery("hive:h_1:done", SLOW).unwrap(), None);
    let record = accepted("hive:h_1:done", 1, "1700000000000000-1");
    journal.record_delivery(&record, SLOW).unwrap();
    drop(journal);
    let journal = Journal::open(&path).unwrap();
    assert_eq!(
        journal.delivery("hive:h_1:done", SLOW).unwrap(),
        Some(record)
    );
    let version: i64 = rusqlite::Connection::open(&path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
}

#[test]
fn a_delivery_record_is_replaced_by_its_event_settled_once_and_deleted_only_with_its_turn() {
    let dir = Scratch::new();
    let mut journal = Journal::open(&db(&dir)).unwrap();
    journal
        .record_delivery(&accepted("e1", 1, "t-1"), SLOW)
        .unwrap();
    journal
        .record_delivery(&accepted("e2", 1, "t-2"), SLOW)
        .unwrap();

    let silent = InboxDeliveryDelivery::Silent;
    assert!(
        !journal.settle_delivery("e1", "t-2", silent, SLOW).unwrap(),
        "another turn"
    );
    assert!(journal.settle_delivery("e1", "t-1", silent, SLOW).unwrap());
    assert!(
        !journal
            .settle_delivery("e1", "t-1", InboxDeliveryDelivery::Sent, SLOW)
            .unwrap(),
        "a second delivery"
    );
    assert_eq!(
        journal.delivery("e1", SLOW).unwrap(),
        Some(InboxDelivery {
            delivery: Some(silent),
            ..accepted("e1", 1, "t-1")
        })
    );

    journal
        .record_delivery(&accepted("e1", 2, "t-3"), SLOW)
        .unwrap();
    assert_eq!(
        journal.delivery("e1", SLOW).unwrap(),
        Some(accepted("e1", 2, "t-3"))
    );
    journal.forget_delivery("e1", "t-1", SLOW).unwrap();
    assert_eq!(
        journal.delivery("e1", SLOW).unwrap(),
        Some(accepted("e1", 2, "t-3"))
    );
    journal.forget_delivery("e1", "t-3", SLOW).unwrap();
    assert_eq!(journal.delivery("e1", SLOW).unwrap(), None);
    assert_eq!(
        journal.delivery("e2", SLOW).unwrap(),
        Some(accepted("e2", 1, "t-2"))
    );
}

#[test]
fn a_turn_after_a_compaction_sends_the_same_bytes_after_a_restart_and_no_archived_message() {
    let dir = Scratch::new();
    let cwd = Scratch::new();
    let path = db(&dir);
    let fake = FakeGateway::start(vec![
        tool_step(vec![call(
            "c1",
            "write_file",
            json!({"path": "notes.txt", "content": "alpha\n"}),
        )]),
        text_step("Wrote it."),
        text_step("Still there."),
    ]);
    let mut engine = Engine::start(&path, &fake, &cwd.0);
    engine.turn("First.", &Interrupt::new());
    engine.turn("Second.", &Interrupt::new());
    drop(fake);
    let session = engine.journal.session(KEY).unwrap().id;
    let archived = engine.journal.messages(session).unwrap();
    assert_eq!(archived.len(), 6);
    let compacted = vec![
        user("Summary: notes.txt was written with alpha and is still there."),
        assistant("Understood."),
    ];
    engine.journal.compact(session, &compacted).unwrap();

    let never_stopped = {
        let fake = FakeGateway::start(vec![text_step("Third answer.")]);
        let mut history = compacted.clone();
        history.push(user("Third."));
        let turn = engine.journal.next_turn(session).unwrap();
        let client = Client::new(fake.socket(), SLOW, SLOW);
        run(&client, &cwd.0, &history, turn, &Interrupt::new(), STEPS);
        turns(&fake)
    };
    drop(engine);

    let restarted = {
        let fake = FakeGateway::start(vec![text_step("Third answer.")]);
        Engine::start(&path, &fake, &cwd.0).turn("Third.", &Interrupt::new());
        turns(&fake)
    };

    assert_eq!(restarted.len(), 1);
    assert_eq!(untagged_bodies(&restarted), untagged_bodies(&never_stopped));
    let body = request(&restarted[0]);
    assert_eq!(untagged(&body.request_id), "s1-t3-0");
    let mut want = compacted;
    want.push(user("Third."));
    assert_eq!(body.messages, want);
    assert!(body.messages.iter().all(|m| !archived.contains(m)));
}
