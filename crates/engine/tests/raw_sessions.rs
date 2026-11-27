//! C6 `raw_sessions` of `ulei-engine serve` on synthetic journals: the turn messages in `n`
//! order with their stored bytes and index text, compaction rows left out, forgotten and
//! untimed rows served, the cursor, the page bounds and a message over them skipped by reference.

#![allow(missing_docs, clippy::unwrap_used)]

mod c6_wire;
mod fake_gateway;

use c6_wire::{Engine, config, ok, post, start};
use fake_gateway::{FakeGateway, Scratch, text_block};
use serde_json::{Value, json};
use ulei_engine::agent::{Compacted, Ended, Outcome, Step};
use ulei_engine::journal::marks::Target;
use ulei_engine::journal::{Journal, Turn};
use ulei_engine::wire::c4g::{Message, Role, StopReason};
use ulei_engine::wire::c6::{RawSessionsPage, RawSkipped};

const AT: i64 = 1_700_000_000;

fn text(role: Role, text: &str) -> Message {
    Message {
        role,
        content: vec![text_block(text)],
    }
}

/// A journal in a scratch directory that a started engine serves afterwards.
struct Dialogue {
    fake: FakeGateway,
    dir: Scratch,
    journal: Journal,
}

impl Dialogue {
    fn new() -> Self {
        let dir = Scratch::new();
        let journal = Journal::open(&dir.path("journal.sqlite3")).unwrap();
        Self {
            fake: FakeGateway::start(vec![]),
            dir,
            journal,
        }
    }

    /// Appends the next turn of `session` at `at`: `input` then `outcome` with one step.
    fn append(&mut self, session: u64, input: &[Message], mut outcome: Outcome, at: i64) -> Turn {
        let turn = self.journal.next_turn(session).unwrap();
        outcome.steps.push(Step {
            request_id: turn.request_ids().unwrap().step(0),
            route: None,
            usage: None,
        });
        self.journal.append(turn, input, &outcome, at).unwrap();
        turn
    }

    /// Appends the next turn of `session` at `at`: `prompt` answered by `reply`.
    fn turn(&mut self, session: u64, prompt: &str, reply: &str, at: i64) -> Turn {
        let out = outcome(vec![text(Role::Assistant, reply)], None);
        self.append(session, &[text(Role::User, prompt)], out, at)
    }

    fn serve(&self) -> Engine {
        let config = config(&self.dir.0, self.fake.socket(), &self.dir.path("c6.sock"));
        start(&self.dir.0, &config)
    }
}

fn outcome(messages: Vec<Message>, compacted: Option<Compacted>) -> Outcome {
    Outcome {
        messages,
        steps: vec![],
        ended: Ended::Stopped(StopReason::End),
        compacted,
    }
}

fn page(engine: &Engine, query: &Value) -> RawSessionsPage {
    serde_json::from_slice(ok(&post(&engine.socket, "raw_sessions", query))).unwrap()
}

/// `(session_key, seq, role, content, ts)` of each message of `page`.
fn rows(page: &RawSessionsPage) -> Vec<(String, u64, String, String, Option<f64>)> {
    page.messages
        .iter()
        .map(|m| {
            (
                m.session_key.clone().unwrap(),
                m.seq.unwrap(),
                m.role.clone(),
                m.content.clone(),
                m.ts,
            )
        })
        .collect()
}

fn row(
    key: &str,
    seq: u64,
    role: &str,
    content: &str,
    at: i64,
) -> (String, u64, String, String, Option<f64>) {
    #[allow(clippy::cast_precision_loss)]
    let ts = Some(at as f64);
    (key.to_owned(), seq, role.to_owned(), content.to_owned(), ts)
}

#[test]
fn the_turn_messages_come_in_n_order_with_their_stored_bytes_and_index_text_and_no_compaction_row()
{
    let mut d = Dialogue::new();
    let owner = d.journal.session("owner").unwrap().id;
    d.journal
        .store_system_prompt(owner, "You are the test assistant.")
        .unwrap();
    d.turn(owner, "Plant the tulips?", "In October.", AT);
    let side = d.journal.session("side").unwrap().id;
    d.turn(side, "Fence paint colour?", "Dark green.", AT + 1);
    d.journal
        .compact(
            owner,
            &[text(Role::User, "Summary of the tulip talk.")],
            AT + 2,
        )
        .unwrap();
    let in_turn = outcome(
        vec![
            text(Role::Assistant, "Bulbs first."),
            text(Role::Assistant, "Then water."),
        ],
        Some(Compacted {
            live: vec![text(Role::User, "Summary of the bulb talk.")],
            after: 1,
            prune_rearm: 0,
        }),
    );
    d.append(
        owner,
        &[text(Role::User, "And the bulbs?")],
        in_turn,
        AT + 3,
    );
    let engine = d.serve();

    let all = page(&engine, &json!({}));
    assert_eq!(
        rows(&all),
        vec![
            row("owner", 0, "user", "Plant the tulips?", AT),
            row("owner", 1, "assistant", "In October.", AT),
            row("side", 0, "user", "Fence paint colour?", AT + 1),
            row("side", 1, "assistant", "Dark green.", AT + 1),
            row("owner", 3, "user", "And the bulbs?", AT + 3),
            row("owner", 4, "assistant", "Bulbs first.", AT + 3),
            row("owner", 6, "assistant", "Then water.", AT + 3),
        ]
    );
    assert_eq!(all.cursor, None);
    let stored = rusqlite::Connection::open(d.dir.path("journal.sqlite3")).unwrap();
    let history: Vec<_> = [owner, side]
        .iter()
        .flat_map(|&s| d.journal.history(s).unwrap())
        .collect();
    for message in &all.messages {
        let (bytes, n): (String, i64) = stored
            .query_row(
                "SELECT message, n FROM messages WHERE session = ?1 AND seq = ?2",
                rusqlite::params![
                    message.session_id.parse::<i64>().unwrap(),
                    message.seq.unwrap().cast_signed()
                ],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(message.message.as_deref(), Some(bytes.as_str()));
        let indexed = history.iter().find(|e| e.n == n.cast_unsigned()).unwrap();
        assert_eq!(
            (message.content.as_str(), message.role.as_str()),
            (indexed.content.as_str(), indexed.role.as_str())
        );
    }
    assert_eq!(all.messages[0].session_id, owner.to_string());
    assert_eq!(all.messages[2].session_id, side.to_string());
}

#[test]
fn the_cursor_pages_through_in_n_order_and_the_last_page_has_none() {
    let mut d = Dialogue::new();
    let owner = d.journal.session("owner").unwrap().id;
    d.turn(owner, "One?", "Two.", AT);
    d.turn(owner, "Three?", "Four.", AT + 1);
    let engine = d.serve();

    let first = page(&engine, &json!({"limit": 3}));
    assert_eq!(first.messages.len(), 3);
    assert_eq!(first.cursor.as_deref(), Some("3"));
    let last = page(&engine, &json!({"limit": 3, "cursor": "3"}));
    assert_eq!(
        rows(&last),
        vec![row("owner", 3, "assistant", "Four.", AT + 1)]
    );
    assert_eq!(last.cursor, None);

    let even = page(&engine, &json!({"limit": 2, "cursor": "2"}));
    assert_eq!(
        even.messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>(),
        ["Three?", "Four."]
    );
    assert_eq!(even.cursor, None, "nothing follows a full last page");
    let past = page(&engine, &json!({"cursor": "4"}));
    assert_eq!((past.messages.len(), past.cursor), (0, None));
}

#[test]
fn a_forgotten_message_is_served_and_a_message_without_time_has_no_ts_and_fails_time_filters() {
    let mut d = Dialogue::new();
    let owner = d.journal.session("owner").unwrap().id;
    d.turn(owner, "Old secret?", "Kept.", AT);
    let by = d.turn(owner, "Forget the old secret.", "Done.", AT + 10);
    d.journal
        .mark(&[Target::Message(1)], "The owner asked.", by, AT + 10)
        .unwrap();
    rusqlite::Connection::open(d.dir.path("journal.sqlite3"))
        .unwrap()
        .execute("UPDATE messages SET at = NULL WHERE n = 2", [])
        .unwrap();
    let engine = d.serve();

    let all = page(&engine, &json!({}));
    assert_eq!(
        all.messages
            .iter()
            .map(|m| (m.content.as_str(), m.ts))
            .collect::<Vec<_>>(),
        [
            ("Old secret?", Some(1_700_000_000.0)),
            ("Kept.", None),
            ("Forget the old secret.", Some(1_700_000_010.0)),
            ("Done.", Some(1_700_000_010.0)),
        ]
    );
    let untimed = serde_json::to_value(&all.messages[1]).unwrap();
    assert!(untimed.get("ts").is_none(), "{untimed}");

    let since = page(&engine, &json!({"since": AT}));
    let until = page(&engine, &json!({"until": AT + 5}));
    let contents = |p: &RawSessionsPage| -> Vec<String> {
        p.messages.iter().map(|m| m.content.clone()).collect()
    };
    assert_eq!(
        contents(&since),
        ["Old secret?", "Forget the old secret.", "Done."]
    );
    assert_eq!(contents(&until), ["Old secret?"]);
}

#[test]
fn a_page_stops_before_the_body_limit_and_a_message_over_it_is_skipped_by_reference() {
    let mut d = Dialogue::new();
    let owner = d.journal.session("owner").unwrap().id;
    let large = "a".repeat(150 * 1024);
    d.turn(owner, &large, &large, AT);
    let huge = "b".repeat(300 * 1024);
    d.turn(owner, &huge, "Short.", AT + 1);
    d.turn(owner, "After.", "Done.", AT + 2);
    let engine = d.serve();

    let first = page(&engine, &json!({}));
    assert_eq!(
        (first.messages.len(), first.skipped, first.cursor.as_deref()),
        (1, None, Some("1"))
    );
    let rest = page(&engine, &json!({"cursor": "1"}));
    let stored: String = rusqlite::Connection::open(d.dir.path("journal.sqlite3"))
        .unwrap()
        .query_row("SELECT message FROM messages WHERE n = 3", [], |r| r.get(0))
        .unwrap();
    #[allow(clippy::cast_precision_loss)]
    let ts = Some((AT + 1) as f64);
    assert_eq!(
        rest.skipped,
        Some(vec![RawSkipped {
            session_id: owner.to_string(),
            session_key: Some("owner".to_owned()),
            seq: Some(2),
            ts,
            len: stored.len() as u64,
        }])
    );
    let contents: Vec<_> = rest.messages.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(contents, [large.as_str(), "Short.", "After.", "Done."]);
    assert_eq!(rest.cursor, None);

    let counted = page(&engine, &json!({"cursor": "2", "limit": 1}));
    assert_eq!(
        (counted.messages.len(), counted.skipped.map(|s| s.len())),
        (0, Some(1)),
        "a skipped message counts toward the limit"
    );
    assert_eq!(counted.cursor.as_deref(), Some("3"));
}

#[test]
fn a_query_outside_the_contract_is_a_bad_request() {
    let d = Dialogue::new();
    let engine = d.serve();
    for query in [
        json!({"cursor": "x"}),
        json!({"cursor": "-1"}),
        json!({"cursor": ""}),
        json!({"limit": 0}),
        json!({"limit": 10_001}),
        json!({"since": -1}),
        json!({"since": 1, "extra": true}),
    ] {
        let answer = post(&engine.socket, "raw_sessions", &query);
        assert_eq!(answer.status, 400, "{query}");
    }
}
