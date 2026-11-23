//! `archive_search` and `archive_read` through the registry on a synthetic journal, a fake C7
//! garden and a fake C10 archive: the tier order, the forget filter of dialogue hits, text and
//! binary originals, and an archive that is unset or unreachable.

#![allow(missing_docs, clippy::unwrap_used)]

mod fake_gateway;

use fake_gateway::{FakeGateway, Scratch, json_bytes, text_block};
use serde_json::{Value, json};
use ulei_engine::agent::{Ended, Outcome};
use ulei_engine::journal::marks::Target;
use ulei_engine::journal::{Journal, Turn};
use ulei_engine::tools::{ArchiveRead, ArchiveSearch, Context, Interrupt, Output, Registry};
use ulei_engine::wire::c4g::{Message, Role, StopReason};
use ulei_engine::wire::{c7, c10};
use ulei_engine::{archive, garden};

const AT: i64 = 1_700_000_000;
const TEXT_ID: &str = "11aa";
const IMAGE_ID: &str = "22bb";

fn text(role: Role, text: &str) -> Message {
    Message {
        role,
        content: vec![text_block(text)],
    }
}

fn entry(source: &str, external_id: &str) -> c10::Entry {
    c10::Entry {
        source: source.into(),
        external_id: external_id.into(),
        who: None,
        at: None,
        kind: "text/plain".into(),
        put_at: 1.0,
    }
}

fn hit(id: &str, entries: Vec<c10::Entry>) -> c10::Hit {
    c10::Hit {
        id: id.into(),
        entries,
        snippet: "the >>>shed<<< key".into(),
        text: Some("the shed key".into()),
        text_truncated: None,
    }
}

fn fact(text: &str) -> c7::Fact {
    c7::Fact {
        fact: text.into(),
        entity: "Shed".into(),
        valid_from: 1_700_000_050.0,
        valid_to: None,
        source_ref: "c8:9".into(),
        score: 1.0,
        fact_id: Some("f:3".into()),
        kind: None,
        domain: None,
        source_kind: None,
        expires_at: None,
        pinned: None,
        proof_count: None,
        recheck_before_acting: None,
        unprocessed: None,
        disputed: None,
        conflicts_with: None,
    }
}

/// Standard base64 with padding.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let group = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, &b)| acc | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() {
                char::from(ALPHABET[(group >> (18 - 6 * i) & 63) as usize])
            } else {
                '='
            });
        }
    }
    out
}

/// A garden answering every `search` with `facts`.
fn fake_garden(facts: Vec<c7::Fact>) -> FakeGateway {
    let fake = FakeGateway::start(vec![]);
    fake.respond(
        "POST /v1/search",
        Box::new(move |_| {
            let answer = c7::SearchResponse {
                facts: facts.clone(),
                lexical_only: None,
            };
            json_bytes(200, &serde_json::to_vec(&answer).unwrap())
        }),
    );
    fake
}

/// An archive answering every `search` with `hits` and a `read` of [`TEXT_ID`] or [`IMAGE_ID`]
/// with a text or a PNG original; any other id is `not_found`.
fn fake_archive(hits: Vec<c10::Hit>) -> FakeGateway {
    let fake = FakeGateway::start(vec![]);
    fake.respond(
        "POST /v1/search",
        Box::new(move |_| {
            let answer = c10::SearchResponse {
                hits: hits.clone(),
                synced: true,
            };
            json_bytes(200, &serde_json::to_vec(&answer).unwrap())
        }),
    );
    fake.respond(
        "POST /v1/read",
        Box::new(|request| {
            let read: c10::ReadRequest = serde_json::from_slice(&request.body).unwrap();
            let (bytes, kind): (&[u8], &str) = match read.id.as_str() {
                TEXT_ID => (
                    "The shed key is under the blue pot.".as_bytes(),
                    "text/plain",
                ),
                IMAGE_ID => (&[0x89, b'P', b'N', b'G', 0, 1, 2], "image/png"),
                _ => {
                    let error = c10::Error {
                        code: c10::ErrorCode::NotFound,
                        message: format!("no item {}", read.id),
                    };
                    return json_bytes(404, &serde_json::to_vec(&error).unwrap());
                }
            };
            let answer = c10::ReadResponse {
                bytes_b64: base64(bytes),
                kind: kind.into(),
                len: bytes.len() as u64,
                entries: vec![entry("notes", "n-1")],
            };
            json_bytes(200, &serde_json::to_vec(&answer).unwrap())
        }),
    );
    fake
}

/// The targets a fake received, in order.
fn targets(fake: &FakeGateway) -> Vec<String> {
    fake.requests()
        .iter()
        .map(fake_gateway::Request::target)
        .collect()
}

/// A journal whose current session is `main`, with the garden and the archive the tools call.
struct World {
    dir: Scratch,
    journal: Journal,
    current: u64,
    garden: Option<garden::Client>,
    archive: Option<archive::Client>,
}

impl World {
    fn new(garden: &FakeGateway, archive: &FakeGateway) -> Self {
        let dir = Scratch::new();
        let mut journal = Journal::open(&dir.path("journal.sqlite3")).unwrap();
        let current = journal.session("main").unwrap().id;
        Self {
            dir,
            journal,
            current,
            garden: Some(garden::Client::new(garden.socket())),
            archive: Some(archive::Client::new(archive.socket())),
        }
    }

    /// Appends the next turn of `session` at `at`: `prompt` answered by `reply`.
    fn turn(&mut self, session: u64, prompt: &str, reply: &str) -> Turn {
        let turn = self.journal.next_turn(session).unwrap();
        let out = Outcome {
            messages: vec![text(Role::Assistant, reply)],
            steps: vec![],
            ended: Ended::Stopped(StopReason::End),
            compacted: None,
        };
        self.journal
            .append(turn, &[text(Role::User, prompt)], &out, AT)
            .unwrap();
        turn
    }

    /// `side/<session>/<seq>` of every message of session `side`, in order, with its `n`.
    fn dialogue_ids(&self, side: u64) -> Vec<(String, u64)> {
        self.journal
            .records(side)
            .unwrap()
            .iter()
            .map(|m| (format!("side/{side}/{}", m.seq), m.n.unwrap()))
            .collect()
    }

    fn forget(&mut self, n: u64) {
        let turn = self.journal.next_turn(self.current).unwrap();
        self.journal
            .mark(&[Target::Message(n)], "Gone.", turn, AT)
            .unwrap();
    }

    fn call(&self, name: &str, input: Value) -> Output {
        let mut registry = Registry::new();
        registry
            .register(Box::new(ArchiveSearch::new(
                self.dir.path("journal.sqlite3"),
                self.current,
                self.garden.clone(),
                self.archive.clone(),
            )))
            .unwrap();
        registry
            .register(Box::new(ArchiveRead::new(self.archive.clone())))
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

    fn ok(&self, name: &str, input: Value) -> Value {
        let out = self.call(name, input);
        assert!(!out.is_error, "{}", out.content);
        serde_json::from_str(&out.content).unwrap()
    }

    fn error(&self, name: &str, input: Value) -> String {
        let out = self.call(name, input);
        assert!(out.is_error, "{}", out.content);
        let body: Value = serde_json::from_str(&out.content).unwrap();
        body["error"].as_str().unwrap().to_owned()
    }
}

const NOT_SEARCHED: &str =
    "the archive was not searched: the garden or the history found something";

#[test]
fn a_garden_fact_answers_without_asking_the_archive_and_says_so() {
    let garden = fake_garden(vec![fact("The shed is painted green.")]);
    let archive = fake_archive(vec![hit(TEXT_ID, vec![entry("notes", "n-1")])]);
    let world = World::new(&garden, &archive);

    let body = world.ok("archive_search", json!({"query": "shed"}));

    assert_eq!(
        body,
        json!({
            "success": true,
            "garden": [{
                "ref": "f:3",
                "fact": "The shed is painted green.",
                "entity": "Shed",
                "when": "2023-11-14T22:14:10Z",
                "source_ref": "c8:9"
            }],
            "history": [],
            "note": NOT_SEARCHED
        })
    );
    assert_eq!(targets(&archive), Vec::<String>::new());
}

#[test]
fn a_history_hit_answers_without_asking_the_archive_and_the_garden_miss_is_an_ordinary_one() {
    let garden = fake_garden(vec![]);
    let archive = fake_archive(vec![hit(TEXT_ID, vec![entry("notes", "n-1")])]);
    let mut world = World::new(&garden, &archive);
    let side = world.journal.new_session("side").unwrap().id;
    world.turn(side, "Where is the shed key?", "Under the pot.");

    let body = world.ok("archive_search", json!({"query": "shed", "limit": 3}));

    let refs: Vec<&str> = body["history"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["ref"].as_str().unwrap())
        .collect();
    assert_eq!(refs, ["c8:1"]);
    assert_eq!(body["note"], NOT_SEARCHED);
    assert_eq!(body.get("archive"), None);
    assert_eq!(targets(&archive), Vec::<String>::new());
    let asked: Vec<c7::SearchRequest> = garden
        .requests()
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(
        asked,
        [c7::SearchRequest {
            q: "shed".into(),
            k: Some(3),
            as_of: None,
            private: Some(true),
            no_miss: None,
        }]
    );
}

#[test]
fn the_archive_is_asked_with_the_filters_when_garden_and_history_find_nothing() {
    let garden = fake_garden(vec![]);
    let archive = fake_archive(vec![hit(TEXT_ID, vec![entry("notes", "n-1")])]);
    let mut world = World::new(&garden, &archive);
    let current = world.current;
    world.turn(current, "The shed key is lost.", "Look under the pot.");

    let body = world.ok(
        "archive_search",
        json!({
            "query": "shed",
            "source": "notes",
            "who": "owner",
            "kind": "text/plain",
            "since": "2023-11-14",
            "until": "2023-11-15",
            "limit": 50
        }),
    );

    assert_eq!(body["history"], json!([]));
    assert_eq!(
        body["archive"],
        json!([{
            "id": TEXT_ID,
            "entries": [{"source": "notes", "external_id": "n-1", "kind": "text/plain"}],
            "snippet": "the >>>shed<<< key",
            "text": "the shed key"
        }])
    );
    assert_eq!(body.get("note"), None);
    let asked: Vec<Value> = archive
        .requests()
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(
        asked,
        [json!({
            "q": "shed",
            "source": "notes",
            "who": "owner",
            "kind": "text/plain",
            "since": 1_699_920_000.0,
            "until": 1_700_092_799.0,
            "limit": 10
        })]
    );
}

#[test]
fn a_forgotten_dialogue_hit_is_left_out_while_the_archive_keeps_its_item() {
    let garden = fake_garden(vec![]);
    let mut world = World::new(&garden, &FakeGateway::start(vec![]));
    let side = world.journal.new_session("side").unwrap().id;
    world.turn(side, "The shed key is under the pot.", "Noted.");
    let ids = world.dialogue_ids(side);
    world.forget(ids[0].1);
    let archive = fake_archive(vec![hit(TEXT_ID, vec![entry("dialogue", &ids[0].0)])]);
    world.archive = Some(archive::Client::new(archive.socket()));

    let body = world.ok("archive_search", json!({"query": "shed"}));
    assert_eq!(body["history"], json!([]));
    assert_eq!(body["archive"], json!([]));

    let read = world.ok("archive_read", json!({"id": TEXT_ID}));
    assert_eq!(read["text"], "The shed key is under the blue pot.");
    assert_eq!(targets(&archive), ["POST /v1/search", "POST /v1/read"]);
}

#[test]
fn a_hit_keeps_its_unforgotten_and_lifted_entries_and_drops_the_forgotten_one() {
    let garden = fake_garden(vec![]);
    let mut world = World::new(&garden, &FakeGateway::start(vec![]));
    let side = world.journal.new_session("side").unwrap().id;
    world.turn(side, "The shed key is under the pot.", "Noted.");
    let ids = world.dialogue_ids(side);
    world.forget(ids[0].1);
    world.forget(ids[1].1);
    let turn = world.journal.next_turn(world.current).unwrap();
    world
        .journal
        .lift(&[Target::Message(ids[1].1)], turn, AT)
        .unwrap();
    let archive = fake_archive(vec![
        hit(
            TEXT_ID,
            vec![entry("dialogue", &ids[0].0), entry("dialogue", &ids[1].0)],
        ),
        hit(
            IMAGE_ID,
            vec![entry("dialogue", &ids[0].0), entry("notes", "n-1")],
        ),
    ]);
    world.archive = Some(archive::Client::new(archive.socket()));

    let body = world.ok("archive_search", json!({"query": "pot"}));

    let kept: Vec<(&str, Vec<&str>)> = body["archive"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            let entries = h["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["external_id"].as_str().unwrap())
                .collect();
            (h["id"].as_str().unwrap(), entries)
        })
        .collect();
    assert_eq!(
        kept,
        [(TEXT_ID, vec![ids[1].0.as_str()]), (IMAGE_ID, vec!["n-1"])]
    );
}

#[test]
fn archive_read_returns_a_text_original_as_text_and_a_binary_one_as_length_and_type() {
    let garden = fake_garden(vec![]);
    let archive = fake_archive(vec![]);
    let world = World::new(&garden, &archive);

    let entries = json!([{"source": "notes", "external_id": "n-1", "kind": "text/plain"}]);
    assert_eq!(
        world.ok("archive_read", json!({"id": TEXT_ID})),
        json!({
            "success": true,
            "id": TEXT_ID,
            "kind": "text/plain",
            "len": 35,
            "entries": entries,
            "text": "The shed key is under the blue pot."
        })
    );
    assert_eq!(
        world.ok("archive_read", json!({"id": IMAGE_ID})),
        json!({
            "success": true,
            "id": IMAGE_ID,
            "kind": "image/png",
            "len": 7,
            "entries": entries,
            "note": "the original is not text; only its length and media type are shown"
        })
    );
    assert_eq!(
        world.error("archive_read", json!({"id": "33cc"})),
        "the archive was not read: the archive refused the call: 404 not_found: no item 33cc"
    );
}

#[test]
fn an_unset_or_unreachable_archive_is_stated_plainly() {
    let garden = fake_garden(vec![]);
    let mut world = World::new(&garden, &FakeGateway::start(vec![]));
    for (archive, why) in [
        (
            None,
            "the archive is not configured (memory.socket is unset)",
        ),
        (
            Some(archive::Client::new(world.dir.path("absent.sock"))),
            "the archive is not reachable (archive socket: ",
        ),
    ] {
        world.archive = archive;

        let said = world.error("archive_search", json!({"query": "shed"}));
        assert!(
            said.starts_with(&format!("the archive was not searched: {why}")),
            "{said}"
        );
        assert!(
            said.ends_with("the garden and the history found nothing"),
            "{said}"
        );

        let said = world.error("archive_read", json!({"id": TEXT_ID}));
        assert!(
            said.starts_with(&format!("the archive was not read: {why}")),
            "{said}"
        );
    }
}
