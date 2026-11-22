//! The `forget` tool through the registry on a synthetic journal: marks and lifts that history
//! search obeys, session refs, candidate queries, refused refs and the owner-turn check.

#![allow(missing_docs, clippy::unwrap_used)]

mod fake_gateway;

use std::path::PathBuf;

use fake_gateway::{Scratch, text_block};
use serde_json::{Value, json};
use ulei_engine::agent::{Ended, Outcome};
use ulei_engine::journal::search::{MessageRole, Search, Sort};
use ulei_engine::journal::{Journal, Turn, unix_now};
use ulei_engine::policy::{Origin, Sender};
use ulei_engine::tools::{Context, Forget, Interrupt, Output, Registry};
use ulei_engine::wire::c4g::{Message, Role, StopReason};

const AT: i64 = 1_700_000_000;
const OWNER: Origin = Origin::TopLevelOwner;

fn text(role: Role, text: &str) -> Message {
    Message {
        role,
        content: vec![text_block(text)],
    }
}

/// A journal file in a scratch directory and a handle on it.
struct History {
    dir: Scratch,
    journal: Journal,
}

impl History {
    fn new() -> Self {
        let dir = Scratch::new();
        let journal = Journal::open(&dir.path("journal.sqlite3")).unwrap();
        Self { dir, journal }
    }

    fn path(&self) -> PathBuf {
        self.dir.path("journal.sqlite3")
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

    /// Calls `forget` with `input` in the running turn of `session` whose prompt has `origin`.
    fn forget(&self, session: u64, origin: Origin, input: Value) -> Output {
        let turn = self.journal.next_turn(session).unwrap();
        let mut registry = Registry::new();
        registry
            .register(Box::new(Forget::new(self.path(), turn, origin)))
            .unwrap();
        let Value::Object(input) = input else {
            panic!("not an object: {input}")
        };
        let interrupt = Interrupt::new();
        let ctx = Context {
            cwd: &self.dir.0,
            interrupt: &interrupt,
        };
        registry.execute("forget", &input, ctx)
    }

    /// `n` of every hit of `search`, ascending.
    fn found(&self, search: &Search<'_>) -> Vec<u64> {
        let mut ns: Vec<u64> = self
            .journal
            .search(search)
            .unwrap()
            .into_iter()
            .map(|h| h.n)
            .collect();
        ns.sort_unstable();
        ns
    }
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

fn answer(out: &Output) -> Value {
    serde_json::from_str(&out.content).unwrap()
}

/// A garden session with messages 1 and 2 and a side session with messages 3 and 4, all
/// mentioning tomatoes.
fn two_sessions() -> (History, u64, u64) {
    let mut history = History::new();
    let garden = history.journal.session("main").unwrap().id;
    let side = history.journal.new_session("side").unwrap().id;
    history.turn(
        garden,
        "Plant tomato seedlings by the fence.",
        "Tomato seedlings go in by May.",
        AT,
    );
    history.turn(
        side,
        "Tomato prices at the market?",
        "Tomato prices rose.",
        AT + 100,
    );
    (history, garden, side)
}

#[test]
fn a_marked_message_is_found_by_no_query_or_filter_and_undo_returns_it_keeping_both_records() {
    let (mut history, garden, side) = two_sessions();
    let before = unix_now();

    let out = history.forget(
        garden,
        OWNER,
        json!({"refs": ["c8:1"], "reason": "The fence plan is off."}),
    );
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(
        answer(&out),
        json!({"success": true, "marked": 1, "already_forgotten": 0})
    );

    for search in [
        find("tomato"),
        find("fence"),
        find("seedlings"),
        Search {
            roles: &[MessageRole::User],
            ..find("tomato")
        },
        Search {
            session: Some(garden),
            ..find("tomato")
        },
        Search {
            since: Some(AT),
            until: Some(AT),
            ..find("tomato")
        },
        Search {
            sort: Sort::Oldest,
            ..find("tomato")
        },
    ] {
        assert!(!history.found(&search).contains(&1), "{search:?}");
    }
    assert_eq!(history.found(&find("tomato")), [2, 3, 4]);

    let mark_turn = history.turn(garden, "Forget the fence plan.", "Done.", AT + 200);
    let marked = history.journal.marks(1).unwrap();
    assert_eq!(marked.len(), 1);
    assert_eq!(marked[0].n, 1);
    assert_eq!(marked[0].reason, "The fence plan is off.");
    assert!(marked[0].at >= before.cast_unsigned());
    assert_eq!(
        marked[0].provenance,
        format!("s{garden}-t{}-n5", mark_turn.number)
    );
    assert_eq!(marked[0].lifted_at, None);

    let out = history.forget(side, OWNER, json!({"refs": ["c8:1"], "undo": true}));
    assert_eq!(answer(&out), json!({"success": true, "lifted": 1}));
    assert_eq!(history.found(&find("tomato")), [1, 2, 3, 4]);
    let lifted = history.journal.marks(1).unwrap();
    assert_eq!(lifted.len(), 1);
    assert_eq!(lifted[0].provenance, marked[0].provenance);
    assert_eq!(lifted[0].reason, marked[0].reason);
    assert!(lifted[0].lifted_at.unwrap() >= marked[0].at);
    assert_eq!(lifted[0].lifted_provenance.as_deref(), Some("s2-t2"));
    history.turn(side, "Bring the fence plan back.", "Back.", AT + 300);
    assert_eq!(
        history.journal.marks(1).unwrap()[0]
            .lifted_provenance
            .as_deref(),
        Some("s2-t2-n7")
    );

    history.forget(
        garden,
        OWNER,
        json!({"refs": ["c8:1"], "reason": "Off again."}),
    );
    let marks = history.journal.marks(1).unwrap();
    assert_eq!(marks.len(), 2);
    assert!(marks[0].lifted_at.is_some());
    assert_eq!(
        (marks[1].reason.as_str(), marks[1].lifted_at),
        ("Off again.", None)
    );
    assert_eq!(history.found(&find("tomato")), [2, 3, 4]);
}

#[test]
fn a_session_ref_marks_the_messages_present_and_a_later_message_of_it_is_found() {
    let (mut history, garden, side) = two_sessions();

    let out = history.forget(
        side,
        OWNER,
        json!({"refs": [format!("c8:session:{garden}"), "c8:2"], "reason": "Old garden talk."}),
    );
    assert_eq!(
        answer(&out),
        json!({"success": true, "marked": 2, "already_forgotten": 0})
    );
    assert_eq!(history.found(&find("tomato")), [3, 4]);

    history.turn(garden, "Tomato blight again.", "Spray copper.", AT + 200);
    assert_eq!(history.found(&find("tomato")), [3, 4, 5]);
    assert_eq!(history.journal.marks(5).unwrap(), []);

    let out = history.forget(
        side,
        OWNER,
        json!({"refs": [format!("c8:session:{garden}")], "reason": "All of it."}),
    );
    assert_eq!(
        answer(&out),
        json!({"success": true, "marked": 2, "already_forgotten": 2})
    );
    assert_eq!(history.found(&find("tomato")), [3, 4]);
}

#[test]
fn query_lists_candidate_refs_within_its_filters_and_marks_nothing() {
    let (history, garden, side) = two_sessions();

    let out = history.forget(garden, OWNER, json!({"query": "tomato seedlings"}));
    assert!(!out.is_error, "{}", out.content);
    let body = answer(&out);
    let refs: Vec<&str> = body["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["ref"].as_str().unwrap())
        .collect();
    assert_eq!(refs.len(), 2);
    assert!(refs.contains(&"c8:1") && refs.contains(&"c8:2"));
    let first = &body["candidates"][0];
    assert_eq!(first["session_id"], json!(garden));
    assert_eq!(first["when"], json!("2023-11-14 22:13:20 UTC"));
    assert!(first["snippet"].as_str().unwrap().contains(">>>"));
    assert!(
        body["note"]
            .as_str()
            .unwrap()
            .starts_with("Nothing was marked.")
    );

    let only = |filters: Value| -> Vec<String> {
        let mut input = json!({"query": "tomato"});
        input
            .as_object_mut()
            .unwrap()
            .extend(filters.as_object().unwrap().clone());
        let out = history.forget(garden, OWNER, input);
        assert!(!out.is_error, "{}", out.content);
        let mut refs: Vec<String> = answer(&out)["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["ref"].as_str().unwrap().to_owned())
            .collect();
        refs.sort_unstable();
        refs
    };
    assert_eq!(only(json!({"session_id": side})), ["c8:3", "c8:4"]);
    assert_eq!(
        only(json!({"since": "2023-11-14T22:13:21Z"})),
        ["c8:3", "c8:4"]
    );
    assert_eq!(
        only(json!({"until": "2023-11-14T23:13:20+01:00"})),
        ["c8:1", "c8:2"]
    );
    assert_eq!(only(json!({"until": "2023-11-14"})).len(), 4);
    assert_eq!(only(json!({"since": "2023-11-15"})), Vec::<String>::new());

    let out = history.forget(
        garden,
        OWNER,
        json!({"query": "tomato", "since": "yesterday"}),
    );
    assert!(out.is_error);
    assert!(out.content.contains("RFC 3339"), "{}", out.content);

    let out = history.forget(
        garden,
        OWNER,
        json!({"query": "tomato", "reason": "Forget them all."}),
    );
    assert!(out.is_error);
    assert_eq!(history.found(&find("tomato")), [1, 2, 3, 4]);
    for n in 1..=4 {
        assert_eq!(history.journal.marks(n).unwrap(), []);
    }
}

#[test]
fn archive_refs_are_refused_seed_refs_are_not_supported_yet_and_neither_marks_anything() {
    let (history, garden, _) = two_sessions();

    let out = history.forget(
        garden,
        OWNER,
        json!({"refs": ["c8:1", "archive:0123abcd"], "reason": "Gone."}),
    );
    assert!(out.is_error);
    assert!(
        out.content.contains("archive:0123abcd refused"),
        "{}",
        out.content
    );

    let out = history.forget(
        garden,
        OWNER,
        json!({"refs": ["c8:1", "seed:42"], "reason": "Gone."}),
    );
    assert!(out.is_error);
    assert!(out.content.contains("not supported yet"), "{}", out.content);

    let out = history.forget(
        garden,
        OWNER,
        json!({"refs": ["c8:1", "c8:99"], "reason": "Gone."}),
    );
    assert!(out.is_error);
    assert!(out.content.contains("no message 99"), "{}", out.content);

    let out = history.forget(garden, OWNER, json!({"refs": ["c8:1"]}));
    assert!(out.is_error);
    assert!(
        out.content.contains("reason is required"),
        "{}",
        out.content
    );

    assert_eq!(history.journal.marks(1).unwrap(), []);
    assert_eq!(history.found(&find("tomato")), [1, 2, 3, 4]);
}

#[test]
fn a_turn_whose_prompt_is_not_the_owners_is_refused_and_marks_nothing() {
    let (history, garden, _) = two_sessions();
    let event = Origin::of(Sender::Event, Some("main"), Some("main"));
    let other_channel = Origin::of(Sender::Owner, Some("side"), Some("main"));

    for origin in [event, other_channel] {
        for input in [
            json!({"refs": ["c8:1"], "reason": "Injected."}),
            json!({"refs": ["c8:1"], "undo": true}),
            json!({"query": "tomato"}),
        ] {
            let out = history.forget(garden, origin, input);
            assert!(out.is_error);
            assert!(out.content.contains("not the owner's"), "{}", out.content);
        }
    }
    assert_eq!(history.journal.marks(1).unwrap(), []);
    assert_eq!(history.found(&find("tomato")), [1, 2, 3, 4]);
}
