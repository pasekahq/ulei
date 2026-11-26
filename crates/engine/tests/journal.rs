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
use ulei_engine::agent::{
    Agent, Compacted, DEFAULT_CONCURRENT_TOOL_TIMEOUT, Ended, Outcome, Step, Template,
};
use ulei_engine::compress::text::SUMMARY_PREFIX;
use ulei_engine::gateway::Client;
use ulei_engine::journal::marks::{Marking, Target};
use ulei_engine::journal::search::{Hit, MAX_QUERY_CHARS, MessageRole, Search, Sort, fts5_query};
use ulei_engine::journal::{Error, Journal, SCHEMA_VERSION, Turn};
use ulei_engine::policy::{Chain, Origin};
use ulei_engine::tools::redact::Redactor;
use ulei_engine::tools::{Interrupt, Registry};
use ulei_engine::wire::c4g::{
    Block, BlockType, Error as GatewayError, ErrorClass, Message, Role, StopReason,
};
use ulei_engine::wire::c8::{InboxDelivery, InboxDeliveryDelivery, Mark, MessageOrigin, Session};

const SLOW: Duration = Duration::from_secs(2);
const KEY: &str = "main";
const STEPS: usize = 16;
const AT: i64 = 1_700_000_000;

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
    journal.append(turn, &[user(prompt)], &out, AT).unwrap();
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
        cache: None,
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
        images: &[],
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
        self.journal
            .append(turn, &[user(prompt)], &out, AT)
            .unwrap();
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
    journal.append(turn, &[user("go")], &first, AT).unwrap();
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
        .append(
            turn,
            &[user("Write.")],
            &outcome(messages.clone(), vec![]),
            AT,
        )
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
        journal.append(turn, &[user("second")], &failing, AT),
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
        journal.append(turn, &[user("again")], &outcome(vec![], vec![]), AT),
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
        journal.append(turn, &[user("First.")], &out, AT).unwrap();
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
            journal.append(turn, &[user(prompt)], &out, AT).unwrap();
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

    journal.compact(id, &summary, AT).unwrap();
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

    journal
        .compact(id, &[user("Summary: all three.")], AT)
        .unwrap();
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
        journal.compact(id, &[user("Summary."), user("refused")], AT),
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
        journal.compact(empty, &[user("Summary.")], AT),
        Err(Error::NoTurn(e)) if e == empty
    ));
    assert_eq!(journal.records(empty).unwrap(), []);
    assert!(matches!(
        journal.compact(99, &[user("Summary.")], AT),
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
        journal.compact(id, &[], AT),
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
    journal.compact(id, &[user("Summary.")], AT).unwrap();
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
    journal.compact(1, &[user("Summary.")], AT).unwrap();
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
    engine.journal.compact(session, &compacted, AT).unwrap();

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

/// The version 5 schema with `rows` (session, seq, turn, message, active) in sessions 1 and 2.
fn version_5_file(path: &Path, rows: &[(i64, i64, i64, &str, bool)]) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
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
CREATE TABLE deliveries (
  event_id TEXT PRIMARY KEY, attempt INTEGER NOT NULL, turn TEXT NOT NULL,
  delivery TEXT) WITHOUT ROWID;
INSERT INTO sessions (key) VALUES ('main'), ('side');
PRAGMA user_version=5;",
    )
    .unwrap();
    for (session, seq, turn, message, active) in rows {
        conn.execute(
            "INSERT INTO messages VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![session, seq, turn, message, active, !active],
        )
        .unwrap();
    }
}

fn json(message: &Message) -> String {
    serde_json::to_string(message).unwrap()
}

/// `(seq, n, at, origin)` of every message record of `session`.
fn numbering(journal: &Journal, session: u64) -> Vec<(u64, u64, Option<u64>, MessageOrigin)> {
    journal
        .records(session)
        .unwrap()
        .into_iter()
        .map(|r| (r.seq, r.n.unwrap(), r.at, r.origin.unwrap()))
        .collect()
}

#[test]
fn a_compacted_version_5_file_migrates_its_summary_head_and_tail_as_compaction() {
    use MessageOrigin::{Compaction, Turn};
    let dir = Scratch::new();
    let path = db(&dir);
    let summary = user(&format!("{SUMMARY_PREFIX}\nThe plan was read."));
    let (head, tail) = (json(&user("Read the plan.")), json(&assistant("Done.")));
    version_5_file(
        &path,
        &[
            (1, 0, 1, &head, false),
            (1, 1, 1, &json(&assistant("Reading.")), false),
            (1, 2, 2, &json(&user("Summarize it.")), false),
            (1, 3, 2, &tail, false),
            (1, 4, 2, &head, true),
            (1, 5, 2, &json(&summary), true),
            (1, 6, 2, &tail, true),
            (1, 7, 3, &json(&user("Next?")), true),
            (1, 8, 3, &json(&assistant("Sure.")), true),
            (2, 0, 1, &tail, true),
            (2, 1, 1, &json(&user("Summarize it.")), true),
        ],
    );

    let journal = Journal::open(&path).unwrap();
    assert_eq!(
        numbering(&journal, 1),
        [
            (0, 1, None, Turn),
            (1, 2, None, Turn),
            (2, 3, None, Turn),
            (3, 4, None, Turn),
            (4, 5, None, Compaction),
            (5, 6, None, Compaction),
            (6, 7, None, Compaction),
            (7, 8, None, Turn),
            (8, 9, None, Turn),
        ]
    );
    assert_eq!(
        numbering(&journal, 2),
        [(0, 10, None, Turn), (1, 11, None, Turn)]
    );
    assert_eq!(
        journal.messages(1).unwrap(),
        [
            user("Read the plan."),
            summary,
            assistant("Done."),
            user("Next?"),
            assistant("Sure.")
        ]
    );
    drop(journal);
    let version: i64 = rusqlite::Connection::open(&path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    assert_eq!(numbering(&Journal::open(&path).unwrap(), 2)[1].1, 11);
}

#[test]
fn a_version_5_file_that_fails_its_migration_keeps_its_version_and_rows() {
    let dir = Scratch::new();
    let path = db(&dir);
    let rows = [
        (1, 0, 1, json(&user("Hello.")), true),
        (1, 1, 1, r#"{"role":"user"}"#.to_owned(), true),
    ];
    let borrowed: Vec<_> = rows
        .iter()
        .map(|(s, q, t, m, a)| (*s, *q, *t, m.as_str(), *a))
        .collect();
    version_5_file(&path, &borrowed);

    assert!(matches!(Journal::open(&path), Err(Error::Json(_))));
    let conn = rusqlite::Connection::open(&path).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 5);
    let stored: Vec<String> = conn
        .prepare("SELECT message FROM messages ORDER BY seq")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(stored, [rows[0].3.clone(), rows[1].3.clone()]);
}

#[test]
fn appends_and_compactions_write_increasing_n_their_time_and_origin() {
    use MessageOrigin::{Compaction, Turn};
    let dir = Scratch::new();
    let mut journal = Journal::open(&db(&dir)).unwrap();
    let id = journal.session(KEY).unwrap().id;
    let other = journal.new_session("side").unwrap().id;

    let turn = journal.next_turn(id).unwrap();
    let out = outcome(vec![assistant("One.")], vec![]);
    journal.append(turn, &[user("First.")], &out, AT).unwrap();
    let turn = journal.next_turn(other).unwrap();
    let out = outcome(vec![assistant("Aside.")], vec![]);
    journal
        .append(turn, &[user("Side.")], &out, AT + 1)
        .unwrap();

    let turn = journal.next_turn(id).unwrap();
    let out = Outcome {
        compacted: Some(Compacted {
            live: vec![user("Summary."), user("Second.")],
            after: 1,
        }),
        ..outcome(vec![assistant("Working."), assistant("Two.")], vec![])
    };
    journal
        .append(turn, &[user("Second.")], &out, AT + 2)
        .unwrap();
    journal
        .compact(id, &[user("Summary again.")], AT + 3)
        .unwrap();

    let at = |t: i64| Some(u64::try_from(t).unwrap());
    assert_eq!(
        numbering(&journal, id),
        [
            (0, 1, at(AT), Turn),
            (1, 2, at(AT), Turn),
            (2, 5, at(AT + 2), Turn),
            (3, 6, at(AT + 2), Turn),
            (4, 7, at(AT + 2), Compaction),
            (5, 8, at(AT + 2), Compaction),
            (6, 9, at(AT + 2), Turn),
            (7, 10, at(AT + 3), Compaction),
        ]
    );
    assert_eq!(
        numbering(&journal, other),
        [(0, 3, at(AT + 1), Turn), (1, 4, at(AT + 1), Turn)]
    );
}

fn find(query: &str) -> Search<'_> {
    Search {
        query,
        roles: &[],
        session: None,
        since: None,
        until: None,
        sort: Sort::Rank,
        limit: 50,
    }
}

/// `n` of every hit, in order.
fn ns(journal: &Journal, search: &Search<'_>) -> Vec<u64> {
    journal
        .search(search)
        .unwrap()
        .into_iter()
        .map(|h| h.n)
        .collect()
}

/// `n` of every hit, ascending.
fn sorted_ns(journal: &Journal, search: &Search<'_>) -> Vec<u64> {
    let mut ns = ns(journal, search);
    ns.sort_unstable();
    ns
}

/// Appends a turn at `at` of `prompt` answered by a `terminal` call whose output is `output`,
/// then by `reply`.
fn append_tool_turn(
    journal: &mut Journal,
    session: u64,
    (prompt, output, reply): (&str, &str, &str),
    at: i64,
) {
    let turn = journal.next_turn(session).unwrap();
    let call = Message {
        role: Role::Assistant,
        content: vec![block(json!({
            "type": "tool_use", "id": "c1", "name": "terminal", "input": {"command": "cargo check"}
        }))],
    };
    let result = Message {
        role: Role::User,
        content: vec![block(
            json!({"type": "tool_result", "id": "c1", "content": output}),
        )],
    };
    let out = outcome(vec![call, result, assistant(reply)], vec![]);
    journal.append(turn, &[user(prompt)], &out, at).unwrap();
}

fn append_at(journal: &mut Journal, session: u64, prompt: &str, reply: &str, at: i64) {
    let turn = journal.next_turn(session).unwrap();
    let out = outcome(vec![assistant(reply)], vec![]);
    journal.append(turn, &[user(prompt)], &out, at).unwrap();
}

fn unix(at: i64) -> u64 {
    u64::try_from(at).unwrap()
}

#[test]
fn each_query_finds_its_own_session_with_id_n_time_and_marked_snippet() {
    let dir = Scratch::new();
    let mut journal = Journal::open(&db(&dir)).unwrap();
    let garden = journal.session(KEY).unwrap().id;
    let rust = journal.new_session("side").unwrap().id;
    append_at(
        &mut journal,
        garden,
        "The garden needs tomato seedlings.",
        "Plant them in May.",
        AT,
    );
    append_at(
        &mut journal,
        rust,
        "A borrow checker question.",
        "Lifetimes explain the borrow checker.",
        AT + 10,
    );

    assert_eq!(
        journal.search(&find("tomato")).unwrap(),
        [Hit {
            session: garden,
            n: 1,
            at: Some(unix(AT)),
            role: MessageRole::User,
            snippet: "The garden needs >>>tomato<<< seedlings.".into(),
        }]
    );
    let mut hits = journal.search(&find("borrow checker")).unwrap();
    hits.sort_by_key(|h| h.n);
    assert_eq!(
        hits,
        [
            Hit {
                session: rust,
                n: 3,
                at: Some(unix(AT + 10)),
                role: MessageRole::User,
                snippet: "A >>>borrow<<< >>>checker<<< question.".into(),
            },
            Hit {
                session: rust,
                n: 4,
                at: Some(unix(AT + 10)),
                role: MessageRole::Assistant,
                snippet: "Lifetimes explain the >>>borrow<<< >>>checker<<<.".into(),
            },
        ]
    );
}

#[test]
fn a_role_filter_keeps_only_its_roles_and_tool_output_is_the_tool_role() {
    let dir = Scratch::new();
    let mut journal = Journal::open(&db(&dir)).unwrap();
    let id = journal.session(KEY).unwrap().id;
    append_tool_turn(
        &mut journal,
        id,
        (
            "Why does the build fail?",
            "error: build failed",
            "The build failed on a typo.",
        ),
        AT,
    );

    assert_eq!(sorted_ns(&journal, &find("build")), [1, 3, 4]);
    let only = |roles: &[MessageRole]| {
        sorted_ns(
            &journal,
            &Search {
                roles,
                ..find("build")
            },
        )
    };
    assert_eq!(only(&[MessageRole::Tool]), [3]);
    assert_eq!(only(&[MessageRole::User, MessageRole::Assistant]), [1, 4]);
    assert_eq!(only(&[MessageRole::Assistant]), [4]);
    assert_eq!(
        sorted_ns(
            &journal,
            &Search {
                roles: &[MessageRole::Tool],
                ..find("terminal")
            }
        ),
        [3]
    );
    assert_eq!(sorted_ns(&journal, &find("terminal")), [2, 3]);
}

#[test]
fn a_session_filter_keeps_only_that_session() {
    let dir = Scratch::new();
    let mut journal = Journal::open(&db(&dir)).unwrap();
    let one = journal.session(KEY).unwrap().id;
    let two = journal.new_session("side").unwrap().id;
    append_text_turn(&mut journal, one, "Water the garden.", "Done.");
    append_text_turn(&mut journal, two, "The garden is dry.", "Noted.");

    assert_eq!(sorted_ns(&journal, &find("garden")), [1, 3]);
    let hits = journal
        .search(&Search {
            session: Some(two),
            ..find("garden")
        })
        .unwrap();
    assert_eq!(
        hits.iter().map(|h| (h.session, h.n)).collect::<Vec<_>>(),
        [(two, 3)]
    );
}

#[test]
fn since_and_until_bound_the_time_of_the_hits_inclusively() {
    let dir = Scratch::new();
    let mut journal = Journal::open(&db(&dir)).unwrap();
    let id = journal.session(KEY).unwrap().id;
    for (prompt, at) in [
        ("First report.", AT),
        ("Second report.", AT + 100),
        ("Third report.", AT + 200),
    ] {
        append_at(&mut journal, id, prompt, "Filed.", at);
    }
    let within = |since, until| {
        sorted_ns(
            &journal,
            &Search {
                since,
                until,
                ..find("report")
            },
        )
    };

    assert_eq!(within(None, None), [1, 3, 5]);
    assert_eq!(within(Some(AT + 100), None), [3, 5]);
    assert_eq!(within(None, Some(AT + 100)), [1, 3]);
    assert_eq!(within(Some(AT + 1), Some(AT + 199)), [3]);
}

#[test]
fn sort_orders_by_time_over_rank() {
    let dir = Scratch::new();
    let mut journal = Journal::open(&db(&dir)).unwrap();
    let id = journal.session(KEY).unwrap().id;
    for (prompt, at) in [
        ("A plan for the week.", AT),
        ("Plan, plan and plan again.", AT + 100),
        ("A plan for the month.", AT + 200),
    ] {
        append_at(&mut journal, id, prompt, "Fine.", at);
    }
    let sorted = |sort| {
        ns(
            &journal,
            &Search {
                sort,
                ..find("plan")
            },
        )
    };

    assert_eq!(sorted(Sort::Rank)[0], 3);
    assert_eq!(sorted(Sort::Newest), [5, 3, 1]);
    assert_eq!(sorted(Sort::Oldest), [1, 3, 5]);
}

#[test]
fn compaction_rows_are_never_found_and_archived_turn_rows_are() {
    let dir = Scratch::new();
    let mut journal = Journal::open(&db(&dir)).unwrap();
    let id = journal.session(KEY).unwrap().id;
    append_text_turn(&mut journal, id, "Alpha first.", "Alpha one.");
    let turn = journal.next_turn(id).unwrap();
    let out = Outcome {
        compacted: Some(Compacted {
            live: vec![user("Alpha summary."), user("Alpha second.")],
            after: 1,
        }),
        ..outcome(
            vec![assistant("Alpha working."), assistant("Alpha two.")],
            vec![],
        )
    };
    journal
        .append(turn, &[user("Alpha second.")], &out, AT + 1)
        .unwrap();
    journal
        .compact(id, &[user("Alpha summary again.")], AT + 2)
        .unwrap();

    let turn_rows: Vec<u64> = journal
        .records(id)
        .unwrap()
        .into_iter()
        .filter(|r| r.origin == Some(MessageOrigin::Turn))
        .map(|r| r.n.unwrap())
        .collect();
    assert_eq!(turn_rows, [1, 2, 3, 4, 7]);
    assert_eq!(sorted_ns(&journal, &find("alpha")), turn_rows);
    assert_eq!(journal.search(&find("summary")).unwrap(), []);
}

/// Turns `path`, a journal of this version, into one of version 6: no history index, no marks.
fn downgrade_to_version_6(path: &Path) {
    rusqlite::Connection::open(path)
        .unwrap()
        .execute_batch("DROP TABLE marks; DROP TABLE messages_fts; PRAGMA user_version=6;")
        .unwrap();
}

#[test]
fn a_version_6_file_migrates_with_its_turn_rows_indexed() {
    let dir = Scratch::new();
    let path = db(&dir);
    let mut journal = Journal::open(&path).unwrap();
    let id = journal.session(KEY).unwrap().id;
    let other = journal.new_session("side").unwrap().id;
    append_tool_turn(
        &mut journal,
        id,
        ("Check the crate.", "warning: unused import", "One warning."),
        AT,
    );
    journal
        .compact(id, &[user("Summary: one warning.")], AT + 1)
        .unwrap();
    append_text_turn(&mut journal, other, "Any warning elsewhere?", "None.");
    let before: Vec<Hit> = journal.search(&find("warning")).unwrap();
    drop(journal);
    downgrade_to_version_6(&path);

    let journal = Journal::open(&path).unwrap();
    assert_eq!(journal.search(&find("warning")).unwrap(), before);
    assert_eq!(sorted_ns(&journal, &find("warning")), [3, 4, 6]);
    assert_eq!(
        sorted_ns(
            &journal,
            &Search {
                roles: &[MessageRole::Tool],
                ..find("terminal")
            }
        ),
        [3]
    );
    drop(journal);
    let version: i64 = rusqlite::Connection::open(&path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
}

#[test]
fn a_version_6_file_that_fails_its_migration_keeps_its_version_and_has_no_index() {
    let dir = Scratch::new();
    let path = db(&dir);
    let mut journal = Journal::open(&path).unwrap();
    let id = journal.session(KEY).unwrap().id;
    append_text_turn(&mut journal, id, "Hello.", "Hi.");
    drop(journal);
    downgrade_to_version_6(&path);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "INSERT INTO messages (session, seq, turn, message, n, origin) VALUES (1, 2, 2, '{\"role\":\"user\"}', 3, 'turn')",
        [],
    )
    .unwrap();
    drop(conn);

    assert!(matches!(Journal::open(&path), Err(Error::Json(_))));
    let conn = rusqlite::Connection::open(&path).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 6);
    let (tables, messages): (i64, i64) = conn
        .query_row(
            "SELECT (SELECT count(*) FROM sqlite_master WHERE name LIKE 'messages_fts%'), (SELECT count(*) FROM messages)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((tables, messages), (0, 3));
}

/// Turns `path`, a journal of this version, into one of version 7: no marks.
fn downgrade_to_version_7(path: &Path) {
    rusqlite::Connection::open(path)
        .unwrap()
        .execute_batch("DROP TABLE marks; PRAGMA user_version=7;")
        .unwrap();
}

fn version(path: &Path) -> i64 {
    rusqlite::Connection::open(path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn a_version_7_file_migrates_with_no_mark_and_its_messages_found_then_marks() {
    let dir = Scratch::new();
    let path = db(&dir);
    let mut journal = Journal::open(&path).unwrap();
    let id = journal.session(KEY).unwrap().id;
    append_text_turn(&mut journal, id, "Seedlings by the fence.", "Fine.");
    drop(journal);
    downgrade_to_version_7(&path);

    let mut journal = Journal::open(&path).unwrap();
    assert_eq!(version(&path), SCHEMA_VERSION);
    assert_eq!(journal.marks(1).unwrap(), []);
    assert_eq!(sorted_ns(&journal, &find("fence")), [1]);
    let by = journal.next_turn(id).unwrap();
    let marking = journal
        .mark(&[Target::Message(1)], "Withdrawn.", by, AT + 1)
        .unwrap();
    assert_eq!(
        marking,
        Marking {
            marked: 1,
            already: 0
        }
    );
    assert_eq!(journal.search(&find("fence")).unwrap(), []);
    assert_eq!(
        journal.marks(1).unwrap(),
        [Mark {
            id: 1,
            n: 1,
            at: unix(AT + 1),
            reason: "Withdrawn.".into(),
            provenance: format!("s{id}-t2"),
            lifted_at: None,
            lifted_provenance: None,
        }]
    );
}

#[test]
fn a_version_7_file_that_fails_its_migration_keeps_its_version_and_rows() {
    let dir = Scratch::new();
    let path = db(&dir);
    let mut journal = Journal::open(&path).unwrap();
    let id = journal.session(KEY).unwrap().id;
    append_text_turn(&mut journal, id, "Seedlings by the fence.", "Fine.");
    drop(journal);
    downgrade_to_version_7(&path);
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE INDEX marks_live ON messages (n);")
        .unwrap();

    assert!(matches!(Journal::open(&path), Err(Error::Sqlite(_))));
    assert_eq!(version(&path), 7);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let (marks, messages): (i64, i64) = conn
        .query_row(
            "SELECT (SELECT count(*) FROM sqlite_master WHERE name = 'marks'), (SELECT count(*) FROM messages)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((marks, messages), (0, 2));
}

#[test]
fn marks_name_existing_messages_and_sessions_or_nothing_is_marked() {
    let dir = Scratch::new();
    let mut journal = Journal::open(&db(&dir)).unwrap();
    let id = journal.session(KEY).unwrap().id;
    append_text_turn(&mut journal, id, "Seedlings by the fence.", "Fine.");
    let by = journal.next_turn(id).unwrap();

    let missing = [Target::Message(1), Target::Message(3)];
    assert!(matches!(
        journal.mark(&missing, "Gone.", by, AT),
        Err(Error::UnknownMessage(3))
    ));
    let missing = [Target::Message(1), Target::Session(id + 1)];
    assert!(matches!(
        journal.lift(&missing, by, AT),
        Err(Error::UnknownSession(_))
    ));
    assert_eq!(journal.marks(1).unwrap(), []);
    assert!(matches!(journal.marks(3), Err(Error::UnknownMessage(3))));
}

#[test]
fn a_current_file_opens_without_reindexing() {
    let dir = Scratch::new();
    let path = db(&dir);
    let mut journal = Journal::open(&path).unwrap();
    let id = journal.session(KEY).unwrap().id;
    append_text_turn(&mut journal, id, "Indexed words.", "Yes.");
    drop(journal);
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute(
            "INSERT INTO messages (session, seq, turn, message, n, origin) VALUES (1, 2, 2, ?1, 3, 'turn')",
            [json(&user("Unindexed words."))],
        )
        .unwrap();

    let journal = Journal::open(&path).unwrap();
    assert_eq!(sorted_ns(&journal, &find("words")), [1]);
    assert_eq!(journal.search(&find("unindexed")).unwrap(), []);
}

#[test]
fn queries_with_fts5_syntax_find_as_the_reference_sanitizes_them_and_never_fail() {
    let dir = Scratch::new();
    let mut journal = Journal::open(&db(&dir)).unwrap();
    let id = journal.session(KEY).unwrap().id;
    append_text_turn(
        &mut journal,
        id,
        "Deploy chat-send today.",
        "The lone wolf howls.",
    );
    append_text_turn(
        &mut journal,
        id,
        "Near alpha beta.",
        "Chat about send later.",
    );

    for (query, found) in [
        ("chat-send", vec![1]),
        ("\"lone", vec![2]),
        ("NEAR(alpha beta)", vec![3]),
        ("(alpha", vec![3]),
        ("\"chat send\"", vec![1]),
        ("***deploy", vec![1]),
        ("depl*", vec![1]),
        ("alpha AND", vec![3]),
        ("OR wolf", vec![2]),
        ("wolf's", vec![]),
        ("hello -world", vec![]),
        ("alpha - beta", vec![]),
        ("alpha AND OR beta", vec![]),
        ("\"", vec![]),
        ("*", vec![]),
    ] {
        assert_eq!(sorted_ns(&journal, &find(query)), found, "{query}");
    }
}

#[test]
fn the_query_sanitizer_gives_the_reference_output() {
    for (query, sanitized) in [
        ("hello -world", "hello -world"),
        ("chat-send", "\"chat-send\""),
        ("\"lone", "lone"),
        ("a \"b c\" d", "a \"b c\" d"),
        ("***x", "x"),
        ("NEAR(a b)", "NEAR a b"),
        ("a AND", "a"),
        ("OR b", "b"),
        ("and alpha", "alpha"),
        ("alpha or", "alpha"),
        ("it's", "it s"),
        ("50%", "50"),
        ("P2.2", "\"P2.2\""),
        ("файл-имя", "\"файл-имя\""),
        ("x\"a\".b", "x\"a\".b"),
        ("a.b.c-d", "\"a.b.c-d\""),
        ("\"a\"*", "\"a\"*"),
        ("AND-x", "-x"),
        ("hello *", "hello"),
        ("alpha* *beta", "alpha* beta"),
        ("q**", "q*"),
        ("a:b", "a b"),
        ("\"", ""),
        ("x\" y", "x  y"),
        ("x'y-z", "x \"y-z\""),
        ("_x-y", "\"_x-y\""),
        ("ALPHA-", "ALPHA-"),
    ] {
        assert_eq!(fts5_query(query), sanitized, "{query}");
    }
    let long = "a ".repeat(MAX_QUERY_CHARS);
    assert_eq!(fts5_query(&long).chars().count(), MAX_QUERY_CHARS - 1);
}

#[test]
fn the_query_sanitizer_takes_the_four_ascii_separators_for_whitespace_as_the_reference() {
    for (query, sanitized) in [
        ("\u{1c}*deploy", "deploy"),
        ("deploy AND\u{1f}", "deploy"),
        ("\u{1d}OR deploy", "deploy"),
        ("\u{1e}deploy\u{1e}", "deploy"),
        ("a\u{1f}*b", "a\u{1f}b"),
    ] {
        assert_eq!(fts5_query(query), sanitized, "{query:?}");
    }
}
