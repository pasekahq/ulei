//! The `forget` tool through the registry on a synthetic journal, a fake C7 garden and a fake C10
//! forest: marks and lifts that history search obeys, every ref sent to the garden, seedbed refs
//! and candidates, forest refs and candidates, the garden or the forest unset or unreachable,
//! refused refs and the owner-turn check.

#![allow(missing_docs, clippy::unwrap_used)]

mod fake_gateway;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

use fake_gateway::{FakeGateway, Scratch, json_bytes, text_block};
use serde_json::{Value, json};
use ulei_engine::agent::{Ended, Outcome};
use ulei_engine::journal::search::{MessageRole, Search, Sort};
use ulei_engine::journal::{Journal, Turn, unix_now};
use ulei_engine::policy::{Origin, Sender};
use ulei_engine::tools::{
    ArchiveSearch, Context, Forget, Interrupt, Output, Registry, SessionSearch,
};
use ulei_engine::wire::c4g::{Message, Role, StopReason};
use ulei_engine::wire::{c7, c10};
use ulei_engine::zone::parse_utc;
use ulei_engine::{archive, garden};

const AT: i64 = 1_700_000_000;
const OWNER: Origin = Origin::TopLevelOwner;

fn text(role: Role, text: &str) -> Message {
    Message {
        role,
        content: vec![text_block(text)],
    }
}

/// Seedbed records the fake garden holds.
const SEEDS: [u64; 2] = [7, 8];
/// Current facts the fake garden queues for a ref.
const FACTS: [(&str, u64); 2] = [("c8:1", 1), ("seed:7", 2)];

/// A C7 garden on a fake socket. `forget` marks and lifts the records of [`SEEDS`] and queues
/// or unqueues the [`FACTS`] of each ref once, as the garden does; an unknown record fails the
/// call with `not_found` and changes nothing. `search` answers the facts of [`found`] whose
/// text holds a word of the query.
struct FakeGarden {
    fake: FakeGateway,
}

impl FakeGarden {
    fn start() -> Self {
        let fake = FakeGateway::start(vec![]);
        let state: Mutex<(HashSet<u64>, HashSet<String>)> = Mutex::default();
        let facts: HashMap<&str, u64> = FACTS.into_iter().collect();
        fake.respond(
            "POST /v1/forget",
            Box::new(move |request| {
                let forget: c7::ForgetRequest = serde_json::from_slice(&request.body).unwrap();
                let seq = |r: &str| r.strip_prefix("seed:").map(|n| n.parse::<u64>().unwrap());
                if let Some(unknown) = forget
                    .refs
                    .iter()
                    .find(|r| seq(r).is_some_and(|n| !SEEDS.contains(&n)))
                {
                    let error = c7::Error {
                        code: c7::ErrorCode::NotFound,
                        message: format!("no record {unknown}"),
                    };
                    return json_bytes(404, &serde_json::to_vec(&error).unwrap());
                }
                let undo = forget.undo == Some(true);
                let mut state = state.lock().unwrap();
                let (marked, queued) = &mut *state;
                let items = forget
                    .refs
                    .iter()
                    .map(|r| {
                        let marks = match seq(r) {
                            Some(n) if undo => u64::from(marked.remove(&n)),
                            Some(n) => u64::from(marked.insert(n)),
                            None => 0,
                        };
                        let changed = if undo {
                            queued.remove(r)
                        } else {
                            queued.insert(r.clone())
                        };
                        let queue = if changed {
                            facts.get(r.as_str()).copied().unwrap_or(0)
                        } else {
                            0
                        };
                        c7::ForgetOutcome {
                            r#ref: r.clone(),
                            marks,
                            queue,
                        }
                    })
                    .collect();
                let answer = c7::ForgetResponse { items };
                json_bytes(200, &serde_json::to_vec(&answer).unwrap())
            }),
        );
        fake.respond(
            "POST /v1/search",
            Box::new(|request| {
                let search: c7::SearchRequest = serde_json::from_slice(&request.body).unwrap();
                let words: Vec<String> =
                    search.q.split_whitespace().map(str::to_lowercase).collect();
                let facts = found()
                    .into_iter()
                    .filter(|f| words.iter().any(|w| f.fact.to_lowercase().contains(w)))
                    .collect();
                let answer = c7::SearchResponse {
                    facts,
                    lexical_only: None,
                };
                json_bytes(200, &serde_json::to_vec(&answer).unwrap())
            }),
        );
        Self { fake }
    }

    fn client(&self) -> garden::Client {
        garden::Client::new(self.fake.socket())
    }

    /// The `forget` requests the garden received, in order.
    fn forgets(&self) -> Vec<c7::ForgetRequest> {
        self.bodies("POST /v1/forget")
            .iter()
            .map(|body| serde_json::from_slice(body).unwrap())
            .collect()
    }

    /// The `search` requests the garden received, in order.
    fn searches(&self) -> Vec<c7::SearchRequest> {
        self.bodies("POST /v1/search")
            .iter()
            .map(|body| serde_json::from_slice(body).unwrap())
            .collect()
    }

    fn bodies(&self, target: &str) -> Vec<Vec<u8>> {
        self.fake
            .requests()
            .into_iter()
            .filter(|r| r.target() == target)
            .map(|r| r.body)
            .collect()
    }
}

/// A garden fact, then the unprocessed seedbed record `seed:7`.
fn found() -> Vec<c7::Fact> {
    let fact = |id: &str, text: &str, source: &str| c7::Fact {
        fact: text.into(),
        entity: "Shed".into(),
        valid_from: 1_700_000_050.0,
        valid_to: None,
        source_ref: source.into(),
        score: 1.0,
        fact_id: Some(id.into()),
        kind: None,
        domain: None,
        source_kind: None,
        expires_at: None,
        pinned: None,
        proof_count: None,
        recheck_before_acting: None,
        unprocessed: id.starts_with("seed:").then_some(true),
        disputed: None,
        conflicts_with: None,
    };
    vec![
        fact("f:3", "The shed is painted green.", "c8:9"),
        fact("seed:7", "The shed key is under the blue pot.", "c8:2"),
    ]
}

/// A journal file in a scratch directory and a handle on it, and the garden `forget` calls.
struct History {
    dir: Scratch,
    journal: Journal,
    fake: FakeGarden,
    garden: Option<garden::Client>,
    forest: Option<archive::Client>,
}

impl History {
    fn new() -> Self {
        let dir = Scratch::new();
        let journal = Journal::open(&dir.path("journal.sqlite3")).unwrap();
        let fake = FakeGarden::start();
        let garden = Some(fake.client());
        Self {
            dir,
            journal,
            fake,
            garden,
            forest: None,
        }
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
        self.call(session, origin, "forget", input)
    }

    /// Calls tool `name` of a registry holding `forget`, `session_search` and `archive_search`
    /// with `input` in the running turn of `session` whose prompt has `origin`.
    fn call(&self, session: u64, origin: Origin, name: &str, input: Value) -> Output {
        let turn = self.journal.next_turn(session).unwrap();
        let mut registry = Registry::new();
        registry
            .register(Box::new(Forget::new(
                self.path(),
                self.garden.clone(),
                self.forest.clone(),
                turn,
                origin,
            )))
            .unwrap();
        registry
            .register(Box::new(SessionSearch::new(self.path(), session)))
            .unwrap();
        registry
            .register(Box::new(ArchiveSearch::new(
                self.path(),
                session,
                self.garden.clone(),
                None,
            )))
            .unwrap();
        let Value::Object(input) = input else {
            panic!("not an object: {input}")
        };
        let interrupt = Interrupt::new();
        let ctx = Context {
            cwd: &self.dir.0,
            interrupt: &interrupt,
        };
        registry.execute(name, &input, ctx)
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

/// The text of an error answer.
fn error(out: &Output) -> String {
    assert!(out.is_error, "{}", out.content);
    answer(out)["error"].as_str().unwrap().to_owned()
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
        json!({"success": true, "marked": 1, "already_forgotten": 0, "queued": 1})
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
    assert_eq!(
        answer(&out),
        json!({"success": true, "lifted": 1, "unqueued": 1})
    );
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
        json!({"success": true, "marked": 2, "already_forgotten": 0, "queued": 1})
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
        json!({"success": true, "marked": 2, "already_forgotten": 2, "queued": 0})
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
    assert_eq!(first["session_id"], json!(garden.to_string()));
    assert_eq!(first["when"], json!("2023-11-14T22:13:20Z"));
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
    assert_eq!(
        only(json!({"session_id": side.to_string()})),
        ["c8:3", "c8:4"]
    );
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

    let padded = format!("00{side}");
    let out = history.forget(
        garden,
        OWNER,
        json!({"query": "tomato", "session_id": padded}),
    );
    assert_eq!(
        error(&out),
        format!("invalid session_id {padded}: a session id without leading zeros")
    );

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
fn a_session_id_and_time_from_forget_or_archive_search_read_that_session_in_session_search() {
    let (mut history, _, side) = two_sessions();
    let now = history.journal.new_session("now").unwrap().id;
    let started = "2023-11-14T22:15:00Z";
    assert_eq!(parse_utc(started, false), Some(AT + 100));
    let ok = |name: &str, input: Value| {
        let out = history.call(now, OWNER, name, input);
        assert!(!out.is_error, "{}", out.content);
        answer(&out)
    };

    let candidate = &ok("forget", json!({"query": "prices"}))["candidates"][0];
    let history_hit = &ok("archive_search", json!({"query": "prices"}))["history"][0];
    for hit in [candidate, history_hit] {
        assert_eq!(hit["when"], started);
        let id = hit["session_id"].as_str().unwrap();
        let read = ok("session_search", json!({"session_id": id}));
        assert_eq!(read["mode"], "read");
        assert_eq!(read["session_id"], side.to_string());
        assert_eq!(read["session_meta"]["when"], started);
        assert_eq!(
            read["messages"][0]["content"],
            "Tomato prices at the market?"
        );
    }
}

#[test]
fn archive_unknown_and_non_canonical_refs_are_refused_and_mark_nothing_anywhere() {
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

    for r in [
        "c8:01",
        "c8:+1",
        "c8:session:01",
        "seed:07",
        "seed:0",
        "c9:1",
    ] {
        let out = history.forget(
            garden,
            OWNER,
            json!({"refs": ["c8:2", r], "reason": "Gone."}),
        );
        assert!(out.is_error, "{r}");
        assert!(out.content.contains(r), "{r}: {}", out.content);
    }

    let out = history.forget(
        garden,
        OWNER,
        json!({"refs": ["c8:1", "c8:99"], "reason": "Gone."}),
    );
    assert!(out.is_error);
    assert!(out.content.contains("no message 99"), "{}", out.content);

    let out = history.forget(
        garden,
        OWNER,
        json!({"refs": ["c8:1", "seed:9"], "reason": "Gone."}),
    );
    assert!(out.is_error);
    assert!(
        out.content
            .contains("seed:9 refused: the garden refused the call: 404 not_found"),
        "{}",
        out.content
    );
    assert!(
        error(&out).ends_with("nothing was marked"),
        "{}",
        out.content
    );

    let out = history.forget(garden, OWNER, json!({"refs": ["c8:1"]}));
    assert!(out.is_error);
    assert!(
        out.content.contains("reason is required"),
        "{}",
        out.content
    );

    let refs: Vec<String> = (1..=101).map(|n| format!("c8:{n}")).collect();
    let out = history.forget(garden, OWNER, json!({"refs": refs, "reason": "Gone."}));
    assert!(out.is_error);
    assert!(out.content.contains("at most 100 refs"), "{}", out.content);

    assert_eq!(history.journal.marks(1).unwrap(), []);
    assert_eq!(history.journal.marks(2).unwrap(), []);
    assert_eq!(history.found(&find("tomato")), [1, 2, 3, 4]);
    let sent: Vec<Vec<String>> = history.fake.forgets().into_iter().map(|f| f.refs).collect();
    assert_eq!(
        sent,
        [["seed:9", "c8:1"]],
        "only the unknown record reached the garden"
    );
}

#[test]
fn marks_and_lifts_go_to_the_garden_with_reason_and_provenance_and_a_session_as_its_messages() {
    let (history, garden, side) = two_sessions();
    let turn = history.journal.next_turn(side).unwrap();
    let provenance = format!("s{side}-t{}", turn.number);

    let out = history.forget(
        side,
        OWNER,
        json!({"refs": ["c8:3", format!("c8:session:{garden}")], "reason": "Old talk."}),
    );
    assert!(!out.is_error, "{}", out.content);
    let out = history.forget(
        side,
        OWNER,
        json!({"refs": [format!("c8:session:{garden}")], "undo": true}),
    );
    assert_eq!(
        answer(&out),
        json!({"success": true, "lifted": 2, "unqueued": 1})
    );

    let refs = |rs: &[&str]| rs.iter().map(|r| (*r).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        history.fake.forgets(),
        [
            c7::ForgetRequest {
                refs: refs(&["c8:3", "c8:1", "c8:2"]),
                reason: Some("Old talk.".into()),
                provenance: provenance.clone(),
                undo: None,
            },
            c7::ForgetRequest {
                refs: refs(&["c8:1", "c8:2"]),
                reason: None,
                provenance,
                undo: Some(true),
            },
        ]
    );
    assert_eq!(history.found(&find("tomato")), [1, 2, 4]);
}

#[test]
fn a_session_of_more_than_a_hundred_messages_goes_to_the_garden_in_requests_of_a_hundred() {
    let mut history = History::new();
    let long = history.journal.session("main").unwrap().id;
    for i in 0..51 {
        history.turn(long, &format!("Note {i}."), "Noted.", AT + i);
    }

    let out = history.forget(
        long,
        OWNER,
        json!({"refs": ["seed:8", format!("c8:session:{long}")], "reason": "All notes."}),
    );
    assert_eq!(
        answer(&out),
        json!({"success": true, "marked": 103, "already_forgotten": 0, "queued": 1})
    );
    let sent: Vec<Vec<String>> = history.fake.forgets().into_iter().map(|f| f.refs).collect();
    let all: Vec<String> = std::iter::once("seed:8".to_owned())
        .chain((1..=102).map(|n| format!("c8:{n}")))
        .collect();
    assert_eq!(sent, [all[..100].to_vec(), all[100..].to_vec()]);
}

#[test]
fn a_seed_ref_is_marked_and_lifted_through_the_garden_beside_journal_refs() {
    let (history, garden, _) = two_sessions();

    let out = history.forget(
        garden,
        OWNER,
        json!({"refs": ["seed:7", "c8:2", "seed:7"], "reason": "Wrong key place."}),
    );
    assert_eq!(
        answer(&out),
        json!({"success": true, "marked": 2, "already_forgotten": 0, "queued": 2})
    );
    let out = history.forget(
        garden,
        OWNER,
        json!({"refs": ["seed:7"], "reason": "Still wrong."}),
    );
    assert_eq!(
        answer(&out),
        json!({"success": true, "marked": 0, "already_forgotten": 1, "queued": 0})
    );
    assert_eq!(history.found(&find("tomato")), [1, 3, 4]);

    let out = history.forget(garden, OWNER, json!({"refs": ["seed:7"], "undo": true}));
    assert_eq!(
        answer(&out),
        json!({"success": true, "lifted": 1, "unqueued": 2})
    );
    let sent: Vec<(Vec<String>, Option<bool>)> = history
        .fake
        .forgets()
        .into_iter()
        .map(|f| (f.refs, f.undo))
        .collect();
    assert_eq!(
        sent,
        [
            (vec!["seed:7".to_owned(), "c8:2".to_owned()], None),
            (vec!["seed:7".to_owned()], None),
            (vec!["seed:7".to_owned()], Some(true)),
        ]
    );
}

#[test]
fn query_lists_the_seedbed_records_the_garden_finds_after_the_history_candidates() {
    let (mut history, garden, _) = two_sessions();
    history.turn(garden, "Where is the shed key?", "Ask later.", AT + 200);

    let out = history.forget(garden, OWNER, json!({"query": "shed"}));
    assert!(!out.is_error, "{}", out.content);
    let body = answer(&out);
    let candidates = body["candidates"].as_array().unwrap();
    let refs: Vec<&str> = candidates
        .iter()
        .map(|c| c["ref"].as_str().unwrap())
        .collect();
    assert_eq!(refs, ["c8:5", "seed:7"]);
    assert_eq!(
        candidates[1],
        json!({
            "ref": "seed:7",
            "when": "2023-11-14T22:14:10Z",
            "source_ref": "c8:2",
            "snippet": "The shed key is under the blue pot."
        })
    );
    assert_eq!(body.get("garden"), None);
    assert_eq!(
        history.fake.searches(),
        [c7::SearchRequest {
            q: "shed".into(),
            k: Some(50),
            as_of: None,
            private: Some(true),
            no_miss: Some(true),
        }]
    );
    assert_eq!(history.fake.forgets(), []);
}

#[test]
fn without_the_garden_query_says_so_a_seed_ref_is_refused_and_journal_marks_are_kept_saying_so() {
    for (unset, line) in [
        (
            true,
            "the garden is not configured (garden.socket is unset)",
        ),
        (false, "the garden is not reachable (garden socket: "),
    ] {
        let (mut history, garden, _) = two_sessions();
        history.garden = if unset {
            None
        } else {
            Some(garden::Client::new(history.dir.path("absent.sock")))
        };

        let out = history.forget(garden, OWNER, json!({"query": "tomato"}));
        assert!(!out.is_error, "{}", out.content);
        let body = answer(&out);
        assert_eq!(body["candidates"].as_array().unwrap().len(), 4);
        let said = body["garden"].as_str().unwrap();
        assert!(said.starts_with(line), "{said}");
        assert!(
            said.ends_with("; seedbed candidates are not listed"),
            "{said}"
        );

        for input in [
            json!({"refs": ["c8:1", "seed:7"], "reason": "Gone."}),
            json!({"refs": ["seed:7"], "undo": true}),
        ] {
            let out = history.forget(garden, OWNER, input);
            assert!(out.is_error);
            assert!(
                error(&out).starts_with(&format!("seed:7 refused: {line}")),
                "{}",
                out.content
            );
        }
        assert_eq!(history.journal.marks(1).unwrap(), []);

        let out = history.forget(
            garden,
            OWNER,
            json!({"refs": [format!("c8:session:{garden}")], "reason": "Gone."}),
        );
        let body = answer(&out);
        assert_eq!(
            (body["marked"].clone(), body.get("queued")),
            (json!(2), None)
        );
        let said = body["garden"].as_str().unwrap();
        assert!(said.starts_with(line), "{said}");
        assert!(
            said.ends_with(
                "; the journal marks are kept, but the garden's forget queue was not updated for 2 of 2 refs"
            ),
            "{said}"
        );
        assert_eq!(history.found(&find("tomato")), [3, 4]);

        let out = history.forget(garden, OWNER, json!({"refs": ["c8:1"], "undo": true}));
        let said = answer(&out)["garden"].as_str().unwrap().to_owned();
        assert!(
            said.ends_with(
                "; the journal marks are lifted, but the garden's forget queue was not updated for 1 of 1 refs"
            ),
            "{said}"
        );
        assert_eq!(history.found(&find("tomato")), [1, 3, 4]);
        assert_eq!(history.fake.forgets(), []);
    }
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
    assert_eq!(history.fake.forgets(), []);
    assert_eq!(history.fake.searches(), []);
}

/// A forest holding records 4 and 5, 5 superseding 4: `forest_forget` marks and lifts the current
/// version, an unknown id is `not_found`; `forest_search` answers record 5 for "shed".
fn fake_forest() -> FakeGateway {
    let fake = FakeGateway::start(vec![]);
    let marked: Mutex<HashSet<u64>> = Mutex::default();
    fake.respond(
        "POST /v1/forest_forget",
        Box::new(move |request| {
            let forget: c10::ForestForgetRequest = serde_json::from_slice(&request.body).unwrap();
            if let Some(unknown) = forget.ids.iter().find(|id| ![4, 5].contains(*id)) {
                let error = c10::Error {
                    code: c10::ErrorCode::NotFound,
                    message: format!("no forest record {unknown}"),
                };
                return json_bytes(404, &serde_json::to_vec(&error).unwrap());
            }
            let mut marked = marked.lock().unwrap();
            let items = forget
                .ids
                .iter()
                .map(|&id| {
                    let changed = if forget.undo == Some(true) {
                        marked.remove(&5)
                    } else {
                        marked.insert(5)
                    };
                    c10::ForestForgotten {
                        id,
                        record: 5,
                        marks: u64::from(changed),
                    }
                })
                .collect();
            let answer = c10::ForestForgetResponse { items };
            json_bytes(200, &serde_json::to_vec(&answer).unwrap())
        }),
    );
    fake.respond(
        "POST /v1/forest_search",
        Box::new(|request| {
            let search: c10::ForestSearchRequest = serde_json::from_slice(&request.body).unwrap();
            let hits = if search.q.contains("shed") {
                vec![c10::ForestHit {
                    id: 5,
                    source: "old-runtime".into(),
                    session: "s-1".into(),
                    text: "Ada asked where the shed key is.".into(),
                    snippet: "where the >>>shed<<< key is".into(),
                    from: Some(1_700_000_000.0),
                    to: Some(1_700_000_060.0),
                    put_at: 1_700_100_000.0,
                    producer: c10::Producer {
                        run: "1700100000-42".into(),
                        route: "small".into(),
                    },
                    links: vec![],
                }]
            } else {
                vec![]
            };
            let answer = c10::ForestSearchResponse { hits };
            json_bytes(200, &serde_json::to_vec(&answer).unwrap())
        }),
    );
    fake
}

/// The `forest_forget` requests a fake forest received, in order.
fn forest_forgets(fake: &FakeGateway) -> Vec<c10::ForestForgetRequest> {
    fake.requests()
        .iter()
        .filter(|r| r.target() == "POST /v1/forest_forget")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

#[test]
fn a_forest_ref_is_marked_and_lifted_through_the_forest_beside_journal_and_seed_refs() {
    let (mut history, garden, _) = two_sessions();
    let forest = fake_forest();
    history.forest = Some(archive::Client::new(forest.socket()));

    let out = history.forget(
        garden,
        OWNER,
        json!({"refs": ["forest:4", "seed:7", "c8:2", "forest:4"], "reason": "Wrong key place."}),
    );
    assert_eq!(
        answer(&out),
        json!({"success": true, "marked": 3, "already_forgotten": 0, "queued": 2})
    );
    let out = history.forget(
        garden,
        OWNER,
        json!({"refs": ["forest:5"], "reason": "Still wrong."}),
    );
    assert_eq!(
        answer(&out),
        json!({"success": true, "marked": 0, "already_forgotten": 1})
    );
    let out = history.forget(garden, OWNER, json!({"refs": ["forest:4"], "undo": true}));
    assert_eq!(answer(&out), json!({"success": true, "lifted": 1}));

    let sent: Vec<(Vec<u64>, Option<String>, Option<bool>)> = forest_forgets(&forest)
        .into_iter()
        .map(|f| (f.ids, f.reason, f.undo))
        .collect();
    assert_eq!(
        sent,
        [
            (vec![4], Some("Wrong key place.".to_owned()), None),
            (vec![5], Some("Still wrong.".to_owned()), None),
            (vec![4], None, Some(true)),
        ]
    );
    assert!(forest_forgets(&forest)[0].provenance.starts_with('s'));
    // The garden never sees a forest ref.
    assert!(
        history
            .fake
            .forgets()
            .iter()
            .all(|f| f.refs.iter().all(|r| !r.starts_with("forest:")))
    );
}

#[test]
fn query_lists_forest_records_and_without_the_forest_a_forest_ref_marks_nothing() {
    let (mut history, garden, _) = two_sessions();
    history.turn(garden, "Where is the shed key?", "Ask later.", AT + 200);
    let forest = fake_forest();
    history.forest = Some(archive::Client::new(forest.socket()));

    let out = history.forget(garden, OWNER, json!({"query": "shed"}));
    let body = answer(&out);
    let candidates = body["candidates"].as_array().unwrap();
    assert_eq!(
        candidates.last().unwrap(),
        &json!({
            "ref": "forest:5",
            "source": "old-runtime",
            "session": "s-1",
            "when": "2023-11-14T22:13:20Z",
            "snippet": "where the >>>shed<<< key is"
        })
    );
    assert_eq!(body.get("forest"), None);

    let out = history.forget(
        garden,
        OWNER,
        json!({"refs": ["forest:9", "c8:1"], "reason": "Gone."}),
    );
    assert!(
        error(&out).starts_with("forest:9 refused: the forest: archive refused the call: 404"),
        "{}",
        out.content
    );
    assert_eq!(history.journal.marks(1).unwrap(), []);

    history.forest = None;
    let out = history.forget(garden, OWNER, json!({"query": "shed"}));
    assert_eq!(
        answer(&out)["forest"],
        "the forest is not configured (memory.socket is unset); forest candidates are not listed"
    );
    let out = history.forget(
        garden,
        OWNER,
        json!({"refs": ["forest:5", "c8:1"], "reason": "Gone."}),
    );
    assert_eq!(
        error(&out),
        "forest:5 refused: the forest is not configured (memory.socket is unset); nothing was marked"
    );
    assert_eq!(history.journal.marks(1).unwrap(), []);
    assert_eq!(history.fake.forgets(), []);

    let out = history.forget(garden, OWNER, json!({"refs": ["forest:05"], "reason": "x"}));
    assert_eq!(
        error(&out),
        "invalid ref forest:05: forest:<id> takes a record id without leading zeros"
    );
}
