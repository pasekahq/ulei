//! The `session_search` tool through the registry on synthetic journals: its four shapes, the
//! current session's live and archived messages, forgotten messages, clamps and bounds.

#![allow(missing_docs, clippy::unwrap_used)]

mod fake_gateway;

use fake_gateway::{Scratch, text_block};
use serde_json::{Value, json};
use ulei_engine::agent::{Ended, Outcome};
use ulei_engine::journal::marks::Target;
use ulei_engine::journal::{Journal, Turn};
use ulei_engine::tools::{Context, Interrupt, Output, Registry, SessionSearch};
use ulei_engine::wire::c4g::{Message, Role, StopReason};

const AT: i64 = 1_700_000_000;
const AT_UTC: &str = "2023-11-14T22:13:20Z";

fn text(role: Role, text: &str) -> Message {
    Message {
        role,
        content: vec![text_block(text)],
    }
}

/// A journal in a scratch directory, the tool run from the turns of session `current`.
struct History {
    dir: Scratch,
    journal: Journal,
    current: u64,
}

impl History {
    /// A journal whose current session is the empty session `main`.
    fn new() -> Self {
        let dir = Scratch::new();
        let mut journal = Journal::open(&dir.path("journal.sqlite3")).unwrap();
        let current = journal.session("main").unwrap().id;
        Self {
            dir,
            journal,
            current,
        }
    }

    fn session(&mut self, key: &str) -> u64 {
        self.journal.new_session(key).unwrap().id
    }

    /// Appends the next turn of `session` at `at`: `prompt` answered by `reply`.
    fn turn(&mut self, session: u64, prompt: &str, reply: &str, at: i64) -> Turn {
        let turn = self.journal.next_turn(session).unwrap();
        let out = Outcome {
            messages: vec![text(Role::Assistant, reply)],
            steps: vec![],
            ended: Ended::Stopped(StopReason::End),
            compacted: None,
        };
        self.journal
            .append(turn, &[text(Role::User, prompt)], &out, at)
            .unwrap();
        turn
    }

    fn call(&self, input: Value) -> Output {
        let mut registry = Registry::new();
        let path = self.dir.path("journal.sqlite3");
        registry
            .register(Box::new(SessionSearch::new(path, self.current)))
            .unwrap();
        let Value::Object(input) = input else {
            panic!("not an object: {input}")
        };
        let interrupt = Interrupt::new();
        let ctx = Context {
            cwd: &self.dir.0,
            interrupt: &interrupt,
        };
        registry.execute("session_search", &input, ctx)
    }

    /// The answer of a call that succeeds.
    fn ok(&self, input: Value) -> Value {
        let out = self.call(input);
        assert!(!out.is_error, "{}", out.content);
        serde_json::from_str(&out.content).unwrap()
    }

    /// The error text of a call that fails.
    fn error(&self, input: Value) -> String {
        let out = self.call(input);
        assert!(out.is_error, "{}", out.content);
        let body: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(body["success"], false);
        body["error"].as_str().unwrap().to_owned()
    }
}

fn message(id: u64, role: &str, content: &str, at: i64) -> Value {
    json!({"id": id, "role": role, "content": content, "timestamp": at})
}

fn anchored(id: u64, role: &str, content: &str, at: i64) -> Value {
    json!({"id": id, "role": role, "content": content, "timestamp": at, "anchor": true})
}

fn ids(messages: &Value) -> Vec<u64> {
    messages
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_u64().unwrap())
        .collect()
}

fn sessions(answer: &Value) -> Vec<String> {
    answer["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["session_id"].as_str().unwrap().to_owned())
        .collect()
}

/// A garden session (messages 1 to 4) and an office session (messages 5 and 6) besides the
/// current one.
fn two_topics() -> (History, u64, u64) {
    let mut h = History::new();
    let garden = h.session("garden");
    let office = h.session("office");
    h.turn(
        garden,
        "Plant tomato seedlings by the fence.",
        "Tomato seedlings go in by May.",
        AT,
    );
    h.turn(garden, "Water them daily?", "Every morning.", AT + 10);
    h.turn(
        office,
        "Send the invoice to the printer.",
        "The invoice is sent.",
        AT + 100,
    );
    (h, garden, office)
}

#[test]
fn discovery_finds_the_session_of_each_topic_with_its_id_and_time() {
    let (h, garden, office) = two_topics();

    let answer = h.ok(json!({"query": "invoice"}));
    assert_eq!(
        answer,
        json!({
            "success": true,
            "mode": "discover",
            "query": "invoice",
            "detail": "adaptive",
            "results": [{
                "session_id": office.to_string(),
                "when": "2023-11-14T22:15:00Z",
                "session_key": "office",
                "matched_role": "assistant",
                "match_message_id": 6,
                "snippet": "The >>>invoice<<< is sent.",
                "bookend_start": [],
                "messages": [
                    message(5, "user", "Send the invoice to the printer.", AT + 100),
                    anchored(6, "assistant", "The invoice is sent.", AT + 100),
                ],
                "bookend_end": [],
                "messages_before": 1,
                "messages_after": 0,
                "detail": "full",
            }],
            "count": 1,
            "sessions_searched": 1,
        })
    );

    let answer = h.ok(json!({"query": "  tomato seedlings  "}));
    assert_eq!(answer["query"], "tomato seedlings");
    assert_eq!(sessions(&answer), [garden.to_string()]);
    let top = &answer["results"][0];
    assert_eq!(top["when"], AT_UTC);
    assert_eq!(top["session_key"], "garden");
    assert_eq!(ids(&top["messages"]), [1, 2, 3, 4]);

    assert_eq!(
        h.ok(json!({"query": "cucumber"})),
        json!({
            "success": true,
            "mode": "discover",
            "query": "cucumber",
            "detail": "adaptive",
            "results": [],
            "count": 0,
            "message": "No matching sessions found.",
        })
    );
}

#[test]
fn adaptive_detail_keeps_only_the_anchor_below_the_top_result_and_full_hydrates_all() {
    let (h, _, _) = two_topics();

    let answer = h.ok(json!({"query": "tomato OR invoice", "sort": "Newest"}));
    assert_eq!(answer["count"], 2);
    let (top, lower) = (&answer["results"][0], &answer["results"][1]);
    assert_eq!(top["match_message_id"], 6);
    assert_eq!(top["detail"], "full");
    assert_eq!(lower["detail"], "compact");
    assert_eq!(lower["bookend_start"], json!([]));
    assert_eq!(lower["bookend_end"], json!([]));
    let anchor = lower["match_message_id"].as_u64().unwrap();
    assert_eq!(ids(&lower["messages"]), [anchor]);
    assert_eq!(lower["messages"][0]["anchor"], true);
    assert_eq!(lower["messages_after"], 4 - anchor);

    let answer = h.ok(json!({"query": "tomato OR invoice", "detail": "FULL"}));
    assert_eq!(answer["detail"], "full");
    for result in answer["results"].as_array().unwrap() {
        assert_eq!(result["detail"], "full");
    }
}

#[test]
fn discovery_bounds_window_messages_to_4000_and_bookends_to_1200_characters() {
    let mut h = History::new();
    let notes = h.session("notes");
    let opening = "o".repeat(1_500);
    h.turn(notes, &opening, "Noted.", AT);
    for i in 1..=5 {
        h.turn(notes, &format!("Filler {i}."), "Fine.", AT + i);
    }
    let long = format!("Quince {}", "q".repeat(4_493));
    h.turn(notes, "Tell me about quince.", &long, AT + 6);
    for i in 7..=12 {
        h.turn(notes, &format!("Filler {i}."), "Fine.", AT + i);
    }

    let top = &h.ok(json!({"query": "quince", "role_filter": "assistant"}))["results"][0];
    assert_eq!(top["match_message_id"], 14);
    let anchor = &top["messages"][5];
    assert_eq!(anchor["id"], 14);
    assert_eq!(anchor["content"], format!("{}…", &long[..4_000]).as_str());
    assert_eq!(anchor["content_truncated"], true);
    assert_eq!(anchor["original_content_chars"], 4_500);
    assert_eq!(ids(&top["messages"]), (9..=19).collect::<Vec<u64>>());
    assert_eq!(ids(&top["bookend_start"]), [1, 2, 3]);
    let first = &top["bookend_start"][0];
    assert_eq!(first["content"], format!("{}…", &opening[..1_200]).as_str());
    assert_eq!(first["original_content_chars"], 1_500);
    assert_eq!(
        top["bookend_start"][1],
        message(2, "assistant", "Noted.", AT)
    );
    assert_eq!(ids(&top["bookend_end"]), [24, 25, 26]);
    assert_eq!(
        (
            top["messages_before"].clone(),
            top["messages_after"].clone()
        ),
        (json!(5), json!(5))
    );
}

#[test]
fn discovery_takes_roles_a_session_and_a_time_range_as_filters() {
    let (mut h, garden, office) = two_topics();
    h.turn(
        garden,
        "Any invoice for the seedlings?",
        "None yet.",
        AT + 200,
    );

    let found = |input: Value| sessions(&h.ok(input));
    assert_eq!(found(json!({"query": "invoice"})).len(), 2);
    assert_eq!(
        found(json!({"query": "invoice", "session_id": garden.to_string()})),
        [garden.to_string()]
    );
    assert_eq!(
        found(json!({"query": "invoice", "since": "2023-11-14T22:16:00Z"})),
        [garden.to_string()]
    );
    assert_eq!(
        found(json!({"query": "invoice", "until": "2023-11-14T22:15:00Z"})),
        [office.to_string()]
    );
    assert_eq!(
        found(json!({"query": "invoice", "role_filter": "tool"})),
        Vec::<String>::new()
    );
    assert_eq!(
        found(json!({"query": "invoice", "role_filter": " robot , "})),
        Vec::<String>::new()
    );
    assert_eq!(
        found(json!({"query": "invoice", "session_id": "abc"})),
        Vec::<String>::new()
    );
    assert_eq!(
        h.error(json!({"query": "invoice", "since": "yesterday"})),
        "since must be an RFC 3339 time or YYYY-MM-DD, not yesterday"
    );
}

#[test]
fn limit_defaults_to_3_and_is_clamped_to_1_through_10_in_discovery_and_browse() {
    let mut h = History::new();
    for i in 0..12 {
        let session = h.session(&format!("plum-{i}"));
        h.turn(session, "Plum jam again.", "Plums.", AT + i);
    }
    let count = |input: Value| h.ok(input)["count"].as_u64().unwrap();

    for (limit, expected) in [
        (json!(null), 3),
        (json!(0), 1),
        (json!(-4), 1),
        (json!(50), 10),
        (json!("5"), 5),
        (json!("many"), 3),
    ] {
        assert_eq!(
            count(json!({"query": "plum", "limit": limit})),
            expected,
            "{limit}"
        );
        assert_eq!(count(json!({"limit": limit})), expected, "{limit}");
    }
}

#[test]
fn scroll_returns_the_window_around_a_message_and_clamps_the_window_to_1_through_20() {
    let mut h = History::new();
    let long = h.session("long");
    for i in 0..15 {
        h.turn(
            long,
            &format!("Question {i}."),
            &format!("Answer {i}."),
            AT + i,
        );
    }
    let other = h.session("other");
    h.turn(other, "Elsewhere.", "Yes.", AT);
    let id = long.to_string();

    let answer =
        h.ok(json!({"session_id": id, "around_message_id": 10, "window": 2, "query": "ignored"}));
    assert_eq!(
        answer,
        json!({
            "success": true,
            "mode": "scroll",
            "session_id": id,
            "around_message_id": 10,
            "session_meta": {"when": AT_UTC, "session_key": "long"},
            "window": 2,
            "messages": [
                message(8, "assistant", "Answer 3.", AT + 3),
                message(9, "user", "Question 4.", AT + 4),
                anchored(10, "assistant", "Answer 4.", AT + 4),
                message(11, "user", "Question 5.", AT + 5),
                message(12, "assistant", "Answer 5.", AT + 5),
            ],
            "messages_before": 2,
            "messages_after": 2,
        })
    );

    let start = h.ok(json!({"session_id": id, "around_message_id": "1"}));
    assert_eq!(start["window"], 5);
    assert_eq!(ids(&start["messages"]), [1, 2, 3, 4, 5, 6]);
    assert_eq!(
        (
            start["messages_before"].clone(),
            start["messages_after"].clone()
        ),
        (json!(0), json!(5))
    );
    let narrow = h.ok(json!({"session_id": id, "around_message_id": 10, "window": 0}));
    assert_eq!(
        (narrow["window"].clone(), ids(&narrow["messages"])),
        (json!(1), vec![9, 10, 11])
    );
    let wide = h.ok(json!({"session_id": id, "around_message_id": 25, "window": 100}));
    assert_eq!(wide["window"], 20);
    assert_eq!(ids(&wide["messages"]), (5..=30).collect::<Vec<u64>>());

    assert_eq!(
        h.error(json!({"session_id": id, "around_message_id": "ten"})),
        "scroll requires integer around_message_id"
    );
    assert_eq!(
        h.error(json!({"session_id": id, "around_message_id": 31})),
        format!("around_message_id 31 not in session_id {id}")
    );
    assert_eq!(
        h.error(json!({"session_id": "99", "around_message_id": 3})),
        "session_id not found: 99"
    );
}

#[test]
fn read_returns_a_session_and_its_first_20_and_last_10_messages_when_longer() {
    let mut h = History::new();
    let short = h.session("short");
    h.turn(short, "Hello there.", "Hi.", AT);
    let long = h.session("long");
    for i in 0..16 {
        h.turn(
            long,
            &format!("Question {i}."),
            &format!("Answer {i}."),
            AT + i,
        );
    }

    assert_eq!(
        h.ok(json!({"session_id": format!(" {short} ")})),
        json!({
            "success": true,
            "mode": "read",
            "session_id": short.to_string(),
            "session_meta": {"when": AT_UTC, "session_key": "short"},
            "message_count": 2,
            "truncated": false,
            "messages": [
                message(1, "user", "Hello there.", AT),
                message(2, "assistant", "Hi.", AT),
            ],
        })
    );

    let answer = h.ok(json!({"session_id": long.to_string(), "limit": 1}));
    assert_eq!(answer["message_count"], 32);
    assert_eq!(answer["truncated"], true);
    let expected: Vec<u64> = (3..=22).chain(25..=34).collect();
    assert_eq!(ids(&answer["messages"]), expected);
    assert_eq!(
        answer["message"],
        "Session has 32 messages; showing first 20 + last 10. Pass around_message_id (any id above) to scroll the middle."
    );
    assert_eq!(
        h.error(json!({"session_id": "7x"})),
        "session_id not found: 7x"
    );
}

#[test]
fn browse_lists_recent_sessions_newest_first_without_the_current_one() {
    let mut h = History::new();
    h.turn(h.current, "The current talk.", "Yes.", AT + 500);
    let old = h.session("old");
    h.turn(old, "An old question.", "Old answer.", AT);
    let recent = h.session("recent");
    let opening = format!("First line\n{}", "x".repeat(80));
    h.turn(recent, &opening, "Long.", AT + 100);
    h.turn(recent, "Second.", "Later.", AT + 300);
    h.session("empty");

    assert_eq!(
        h.ok(json!({"query": "   "})),
        json!({
            "success": true,
            "mode": "browse",
            "results": [
                {
                    "session_id": recent.to_string(),
                    "session_key": "recent",
                    "started_at": AT + 100,
                    "last_active": AT + 300,
                    "message_count": 4,
                    "preview": format!("First line {}...", "x".repeat(49)),
                },
                {
                    "session_id": old.to_string(),
                    "session_key": "old",
                    "started_at": AT,
                    "last_active": AT,
                    "message_count": 2,
                    "preview": "An old question.",
                },
            ],
            "count": 2,
            "message": "Showing 2 most recent sessions. Pass a query= to search, or session_id+around_message_id to scroll.",
        })
    );
}

/// The current session with its first turn archived by a compaction and a live second turn,
/// and a side session; every turn mentions apricots.
fn current_with_archive() -> (History, u64) {
    let mut h = History::new();
    let current = h.current;
    h.turn(
        current,
        "Apricot jam recipe?",
        "Boil apricots with sugar.",
        AT,
    );
    h.journal
        .compact(
            current,
            &[text(Role::User, "Summary of the apricot talk.")],
            AT + 1,
        )
        .unwrap();
    h.turn(
        current,
        "Apricot pie instead?",
        "Bake apricots in dough.",
        AT + 2,
    );
    let side = h.session("side");
    h.turn(side, "Apricot prices?", "Apricots are cheap.", AT + 3);
    (h, side)
}

#[test]
fn the_current_sessions_live_messages_are_skipped_and_its_archived_ones_found() {
    let (h, side) = current_with_archive();
    let current = h.current.to_string();

    let answer = h.ok(json!({"query": "apricot", "limit": 10, "detail": "full"}));
    let mut found = sessions(&answer);
    found.sort();
    assert_eq!(found, [current.clone(), side.to_string()]);
    let own = answer["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["session_id"] == current.as_str())
        .unwrap();
    assert_eq!(own["match_message_id"], 1);
    assert_eq!(ids(&own["messages"]), [1, 2, 4, 5]);
    assert_eq!(
        sessions(&h.ok(json!({"query": "pie", "role_filter": "user"}))),
        Vec::<String>::new()
    );

    let scrolled = h.ok(json!({"session_id": current, "around_message_id": 2, "window": 1}));
    assert_eq!(ids(&scrolled["messages"]), [1, 2, 4]);
    for anchor in [4, 3, 99] {
        assert_eq!(
            h.error(json!({"session_id": current, "around_message_id": anchor})),
            "scroll rejected: anchor lives in the current session lineage (already in your active context)",
            "{anchor}"
        );
    }
    let browsed = sessions(&h.ok(json!({"limit": 10})));
    assert_eq!(browsed, [side.to_string()]);
}

#[test]
fn a_forgotten_message_is_found_by_no_shape() {
    let (mut h, garden, _) = two_topics();
    let by = h.journal.next_turn(h.current).unwrap();
    h.journal
        .mark(&[Target::Message(1)], "Not current.", by, AT + 500)
        .unwrap();
    let garden_id = garden.to_string();

    assert_eq!(h.ok(json!({"query": "fence"}))["count"], 0);
    let top = &h.ok(json!({"query": "seedlings"}))["results"][0];
    assert_eq!(top["match_message_id"], 2);
    assert_eq!(ids(&top["messages"]), [2, 3, 4]);
    assert_eq!(top["messages_before"], 0);
    let scrolled = h.ok(json!({"session_id": garden_id, "around_message_id": 2}));
    assert_eq!(ids(&scrolled["messages"]), [2, 3, 4]);
    assert_eq!(
        h.error(json!({"session_id": garden_id, "around_message_id": 1})),
        format!("around_message_id 1 not in session_id {garden_id}")
    );
    let read = h.ok(json!({"session_id": garden_id}));
    assert_eq!(
        (read["message_count"].clone(), ids(&read["messages"])),
        (json!(3), vec![2, 3, 4])
    );
    let listed = &h.ok(json!({"limit": 10}))["results"];
    let garden_row = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["session_id"] == garden_id.as_str())
        .unwrap();
    assert_eq!(garden_row["message_count"], 3);
    assert_eq!(garden_row["preview"], "Water them daily?");
    assert_eq!(garden_row["started_at"], AT);

    h.journal
        .mark(&[Target::Session(garden)], "All of it.", by, AT + 501)
        .unwrap();
    assert_eq!(sessions(&h.ok(json!({"limit": 10}))).len(), 1);
}

#[test]
fn queries_with_fts5_syntax_characters_find_without_error() {
    let (h, garden, _) = two_topics();

    for query in [
        "tomato\"",
        "(tomato",
        "tomato:",
        "AND tomato OR",
        "tomato*** {seedlings}",
        "tomato%",
        "\"tomato seedlings\"",
    ] {
        assert_eq!(
            sessions(&h.ok(json!({"query": query}))),
            [garden.to_string()],
            "{query:?}"
        );
    }
    assert_eq!(h.ok(json!({"query": "NOT"}))["count"], 0);
}
