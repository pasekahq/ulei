//! `archive_search` and `archive_read` through the registry on a synthetic journal, a fake C7
//! garden and a fake C10 archive with its forest: the tier order garden, forest, history, archive
//! (ADR-0079), the forget filter of dialogue hits and forest records, the lift of
//! archive hits into the seedbed, text and binary originals, and an archive that is unset or
//! unreachable. One test lifts into the workspace-built `ulei-garden serve`.

#![allow(missing_docs, clippy::unwrap_used)]

mod fake_gateway;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

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
        conflict_note: None,
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

/// A garden answering every `search` with `facts` and the n-th `propose` `seeded` as `seed:<n>`.
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
    let next = AtomicU64::new(1);
    fake.respond(
        "POST /v1/propose",
        Box::new(move |_| {
            let seq = next.fetch_add(1, Ordering::Relaxed);
            json_bytes(200, &serde_json::to_vec(&seeded(seq)).unwrap())
        }),
    );
    fake
}

fn seeded(seq: u64) -> c7::ProposeResponse {
    c7::ProposeResponse {
        proposal_id: format!("seed:{seq}"),
        status: c7::ProposalStatus::Seeded,
        fact_id: None,
        reason: None,
        conflicts_with: None,
    }
}

/// The `propose` requests a fake garden received, in order.
fn proposed(garden: &FakeGateway) -> Vec<c7::ProposeRequest> {
    garden
        .requests()
        .iter()
        .filter(|r| r.target() == "POST /v1/propose")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

/// An archive answering every `search` with `hits`, every `forest_search` with no record and a
/// `read` of [`TEXT_ID`] or [`IMAGE_ID`] with a text or a PNG original; any other id is
/// `not_found`.
fn fake_archive(hits: Vec<c10::Hit>) -> FakeGateway {
    fake_memory(hits, vec![])
}

/// [`fake_archive`] whose `forest_search` answers `forest`.
fn fake_memory(hits: Vec<c10::Hit>, forest: Vec<c10::ForestHit>) -> FakeGateway {
    let fake = FakeGateway::start(vec![]);
    fake.respond(
        "POST /v1/forest_search",
        Box::new(move |_| {
            let answer = c10::ForestSearchResponse {
                hits: forest.clone(),
            };
            json_bytes(200, &serde_json::to_vec(&answer).unwrap())
        }),
    );
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
                offset: None,
                entries: vec![entry("notes", "n-1")],
            };
            json_bytes(200, &serde_json::to_vec(&answer).unwrap())
        }),
    );
    fake
}

/// The bodies of the requests for `target` a fake received, in order.
fn bodies(fake: &FakeGateway, target: &str) -> Vec<Value> {
    fake.requests()
        .iter()
        .filter(|r| r.target() == target)
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

/// A forest record of `source` and `session` compressing the entries `links`.
fn record(id: u64, source: &str, session: &str, links: &[&str]) -> c10::ForestHit {
    c10::ForestHit {
        id,
        source: source.into(),
        session: session.into(),
        text: "Ada asked where the shed key is; it is under the blue pot.".into(),
        snippet: "where the >>>shed<<< key is".into(),
        from: Some(1_700_000_000.0),
        to: Some(1_700_000_060.0),
        put_at: 1_700_100_000.0,
        producer: c10::Producer {
            run: "1700100000-42".into(),
            route: "small".into(),
        },
        links: links
            .iter()
            .enumerate()
            .map(|(i, external_id)| c10::ForestLinked {
                seq: i as u64 + 1,
                id: format!("{:064x}", i + 1),
                external_id: (*external_id).into(),
            })
            .collect(),
    }
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
    "the archive was not searched: the garden, the forest or the history found something";

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
            "forest": [],
            "history": [],
            "note": NOT_SEARCHED
        })
    );
    assert_eq!(targets(&archive), ["POST /v1/forest_search"]);
    assert_eq!(targets(&garden), ["POST /v1/search"]);
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
    assert_eq!(targets(&archive), ["POST /v1/forest_search"]);
    assert_eq!(targets(&garden), ["POST /v1/search"]);
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
            "seed": "seed:1",
            "entries": [{"source": "notes", "external_id": "n-1", "kind": "text/plain"}],
            "snippet": "the >>>shed<<< key",
            "text": "the shed key"
        }])
    );
    assert_eq!(body.get("note"), None);
    assert_eq!(
        bodies(&archive, "POST /v1/forest_search"),
        [json!({
            "q": "shed",
            "source": "notes",
            "since": 1_699_920_000.0,
            "until": 1_700_092_799.0,
            "limit": 10
        })]
    );
    assert_eq!(
        bodies(&archive, "POST /v1/search"),
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
    assert_eq!(body.get("lift_error"), None);
    assert_eq!(targets(&garden), ["POST /v1/search"]);

    let read = world.ok("archive_read", json!({"id": TEXT_ID}));
    assert_eq!(read["text"], "The shed key is under the blue pot.");
    assert_eq!(
        targets(&archive),
        ["POST /v1/forest_search", "POST /v1/search", "POST /v1/read"]
    );
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
fn every_archive_hit_is_proposed_as_a_raw_seedbed_record_and_shows_its_seed_ref() {
    let garden = fake_garden(vec![]);
    let at = |at: f64, external_id: &str| c10::Entry {
        at: Some(at),
        ..entry("notes", external_id)
    };
    let mut untimed = hit(IMAGE_ID, vec![entry("notes", "n-3")]);
    untimed.text = Some("a picture of the shed".into());
    let archive = fake_archive(vec![
        hit(
            TEXT_ID,
            vec![at(1_700_000_200.0, "n-1"), at(1_700_000_100.0, "n-2")],
        ),
        untimed,
    ]);
    let world = World::new(&garden, &archive);

    let body = world.ok("archive_search", json!({"query": "shed"}));

    let seeds: Vec<(&str, &str)> = body["archive"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| (h["id"].as_str().unwrap(), h["seed"].as_str().unwrap()))
        .collect();
    assert_eq!(seeds, [(TEXT_ID, "seed:1"), (IMAGE_ID, "seed:2")]);
    assert_eq!(body.get("lift_error"), None);
    let candidate = |fact: &str, source_ref: &str, observed_at| c7::ProposeRequest {
        fact: fact.into(),
        entity: None,
        source_ref: source_ref.into(),
        valid_from: None,
        kind: None,
        domain: None,
        attribute: None,
        valid_to: None,
        source_kind: Some(c7::SourceKind::Raw),
        observed_at,
        confidence: None,
        ttl_kind: None,
        secret_ref: None,
        private: None,
        pin: None,
    };
    assert_eq!(
        proposed(&garden),
        [
            candidate("the shed key", "archive:11aa", Some(1_700_000_100.0)),
            candidate("a picture of the shed", "archive:22bb", None),
        ]
    );
}

#[test]
fn a_refused_hit_and_a_failed_call_leave_the_hits_shown_and_say_how_many_and_why() {
    let garden = fake_garden(vec![]);
    let refused = c7::ProposeResponse {
        proposal_id: "p:9".into(),
        status: c7::ProposalStatus::Rejected,
        reason: Some(c7::ProposalReason::SecretValue),
        ..seeded(0)
    };
    garden.answer_once(
        "POST /v1/propose",
        json_bytes(200, &serde_json::to_vec(&refused).unwrap()),
    );
    let failed = c7::Error {
        code: c7::ErrorCode::StoreFailed,
        message: "store is busy".into(),
    };
    garden.answer_once(
        "POST /v1/propose",
        json_bytes(200, &serde_json::to_vec(&seeded(4)).unwrap()),
    );
    garden.answer_once(
        "POST /v1/propose",
        json_bytes(500, &serde_json::to_vec(&failed).unwrap()),
    );
    let ids = ["01", "02", "03", "04"];
    let archive = fake_archive(
        ids.iter()
            .map(|id| hit(id, vec![entry("notes", id)]))
            .collect(),
    );
    let world = World::new(&garden, &archive);

    let body = world.ok("archive_search", json!({"query": "shed"}));

    let shown: Vec<(&str, Option<&str>)> = body["archive"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| (h["id"].as_str().unwrap(), h["seed"].as_str()))
        .collect();
    assert_eq!(
        shown,
        [
            ("01", None),
            ("02", Some("seed:4")),
            ("03", None),
            ("04", None)
        ]
    );
    assert_eq!(
        body["lift_error"],
        "3 of 4 archive hits were not lifted into the seedbed: 01 was refused by the garden: \
         secret_value; the garden refused the call: 500 store_failed: store is busy"
    );
    assert_eq!(proposed(&garden).len(), 3);
}

#[test]
fn an_unreachable_or_unset_garden_still_returns_the_archive_hits_and_says_the_lift_failed() {
    let archive = fake_archive(vec![
        hit(TEXT_ID, vec![entry("notes", "n-1")]),
        hit(IMAGE_ID, vec![entry("notes", "n-2")]),
    ]);
    let mut world = World::new(&fake_garden(vec![]), &archive);
    for (garden, why) in [
        (
            Some(garden::Client::new(world.dir.path("absent.sock"))),
            "the garden is not reachable (garden socket: ",
        ),
        (
            None,
            "the garden is not configured (garden.socket is unset)",
        ),
    ] {
        world.garden = garden;

        let body = world.ok("archive_search", json!({"query": "shed"}));

        let ids: Vec<&str> = body["archive"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| {
                assert_eq!(h.get("seed"), None);
                h["id"].as_str().unwrap()
            })
            .collect();
        assert_eq!(ids, [TEXT_ID, IMAGE_ID]);
        let said = body["lift_error"].as_str().unwrap();
        assert!(
            said.starts_with(&format!(
                "2 of 2 archive hits were not lifted into the seedbed: {why}"
            )),
            "{said}"
        );
    }
}

/// The `ulei-garden` binary of this build: in the target directory that holds this test.
fn garden_binary() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    exe.ancestors()
        .map(|dir| dir.join("ulei-garden"))
        .find(|path| path.is_file())
        .unwrap_or_else(|| {
            panic!(
                "no ulei-garden above {}; build the workspace (cargo test --workspace)",
                exe.display()
            )
        })
}

/// `ulei-garden serve` on a fresh store in `dir`, killed on drop.
struct Garden(Child);

impl Garden {
    fn start(dir: &Scratch) -> Self {
        let key = dir.path("producer.key");
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&key)
            .unwrap()
            .write_all(&[0x5a; 32])
            .unwrap();
        let config = dir.path("garden.toml");
        fs::write(
            &config,
            format!(
                "[garden]\nstore = \"{}\"\nsocket = \"{}\"\nproducer_key_file = \"{}\"\n\n\
                 [inbox]\nsocket = \"{}\"\n",
                dir.path("garden.sqlite3").display(),
                dir.path("garden.sock").display(),
                key.display(),
                dir.path("inbox.sock").display(),
            ),
        )
        .unwrap();
        let mut child = Command::new(garden_binary())
            .args(["serve", "--config"])
            .arg(&config)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert!(line.starts_with("ulei-garden ready /"), "{line:?}");
        Self(child)
    }
}

impl Drop for Garden {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn a_lifted_hit_is_one_unprocessed_record_of_the_real_garden_however_often_it_is_found() {
    let store = Scratch::new();
    let _daemon = Garden::start(&store);
    let real = garden::Client::new(store.path("garden.sock"));
    let mut lost = hit(TEXT_ID, vec![entry("notes", "n-1")]);
    lost.text = Some("The shed key hangs behind the blue door.".into());
    lost.entries[0].at = Some(1_700_000_000.0);
    let archive = fake_archive(vec![lost]);
    let mut world = World::new(&fake_garden(vec![]), &archive);
    world.garden = Some(real.clone());

    let first = world.ok("archive_search", json!({"query": "shed"}));
    let again = world.ok("archive_search", json!({"query": "lantern"}));

    assert_eq!(
        targets(&archive),
        [
            "POST /v1/forest_search",
            "POST /v1/search",
            "POST /v1/forest_search",
            "POST /v1/search"
        ]
    );
    let seed = first["archive"][0]["seed"].as_str().unwrap();
    assert!(seed.starts_with("seed:"), "{first}");
    assert_eq!(again["archive"][0]["seed"], seed);
    let found = real
        .search(&c7::SearchRequest {
            q: "shed key blue door".into(),
            k: Some(10),
            as_of: None,
            private: Some(true),
            no_miss: Some(true),
        })
        .unwrap();
    let records: Vec<Value> = found
        .facts
        .iter()
        .map(|f| {
            json!({
                "ref": f.fact_id,
                "source_ref": f.source_ref,
                "unprocessed": f.unprocessed,
                "source_kind": f.source_kind,
            })
        })
        .collect();
    assert_eq!(
        records,
        [json!({
            "ref": seed,
            "source_ref": "archive:11aa",
            "unprocessed": true,
            "source_kind": "raw",
        })]
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
            said.ends_with("the garden, the forest and the history found nothing"),
            "{said}"
        );

        let said = world.error("archive_read", json!({"id": TEXT_ID}));
        assert!(
            said.starts_with(&format!("the archive was not read: {why}")),
            "{said}"
        );
    }
}

#[test]
fn a_forest_hit_answers_after_an_empty_garden_without_asking_the_archive() {
    let garden = fake_garden(vec![]);
    let archive = fake_memory(
        vec![hit(TEXT_ID, vec![entry("notes", "n-1")])],
        vec![record(4, "old-runtime", "s-1", &["s-1/m-1", "s-1/m-2"])],
    );
    let world = World::new(&garden, &archive);

    let body = world.ok("archive_search", json!({"query": "shed"}));

    assert_eq!(
        body,
        json!({
            "success": true,
            "garden": [],
            "forest": [{
                "ref": "forest:4",
                "source": "old-runtime",
                "session": "s-1",
                "from": "2023-11-14T22:13:20Z",
                "to": "2023-11-14T22:14:20Z",
                "text": "Ada asked where the shed key is; it is under the blue pot.",
                "archive": [format!("archive:{:064x}", 1), format!("archive:{:064x}", 2)],
                "links": 2
            }],
            "history": [],
            "note": NOT_SEARCHED
        })
    );
    assert_eq!(targets(&archive), ["POST /v1/forest_search"]);
    assert_eq!(targets(&garden), ["POST /v1/search"]);
}

#[test]
fn an_empty_forest_lets_the_archive_be_asked_and_a_failed_forest_is_named() {
    let garden = fake_garden(vec![]);
    let archive = fake_memory(vec![hit(TEXT_ID, vec![entry("notes", "n-1")])], vec![]);
    let mut world = World::new(&garden, &archive);

    let body = world.ok("archive_search", json!({"query": "shed"}));
    assert_eq!(body["forest"], json!([]));
    assert_eq!(body.get("forest_error"), None);
    assert_eq!(body["archive"][0]["id"], TEXT_ID);
    assert_eq!(
        targets(&archive),
        ["POST /v1/forest_search", "POST /v1/search"]
    );

    // An archive without the forest's methods: the forest's failure is a line, not a failure.
    let old = fake_archive_without_forest(vec![hit(TEXT_ID, vec![entry("notes", "n-1")])]);
    world.archive = Some(archive::Client::new(old.socket()));
    let body = world.ok("archive_search", json!({"query": "shed"}));
    let said = body["forest_error"].as_str().unwrap();
    assert!(
        said.starts_with("the forest: archive refused the call: 404"),
        "{said}"
    );
    assert_eq!(body["archive"][0]["id"], TEXT_ID);
}

#[test]
fn a_forest_record_of_a_forgotten_dialogue_message_is_left_out_and_the_archive_is_asked() {
    let garden = fake_garden(vec![]);
    let mut world = World::new(&garden, &FakeGateway::start(vec![]));
    let side = world.journal.new_session("side").unwrap().id;
    world.turn(side, "The lantern is in the attic.", "Noted.");
    let ids = world.dialogue_ids(side);
    world.forget(ids[0].1);
    let forgotten = record(
        5,
        "dialogue",
        &format!("side/{side}"),
        &[&ids[0].0, &ids[1].0],
    );
    let kept = record(6, "dialogue", &format!("side/{side}"), &[&ids[1].0]);
    let archive = fake_memory(
        vec![hit(TEXT_ID, vec![entry("notes", "n-1")])],
        vec![forgotten],
    );
    world.archive = Some(archive::Client::new(archive.socket()));

    let body = world.ok("archive_search", json!({"query": "shed"}));
    assert_eq!(body["forest"], json!([]));
    assert_eq!(body["archive"][0]["id"], TEXT_ID);

    let archive = fake_memory(vec![], vec![kept]);
    world.archive = Some(archive::Client::new(archive.socket()));
    let body = world.ok("archive_search", json!({"query": "shed"}));
    assert_eq!(body["forest"][0]["ref"], "forest:6");
    assert_eq!(targets(&archive), ["POST /v1/forest_search"]);
}

/// An archive of C10 before the forest: `forest_search` is an unknown method.
fn fake_archive_without_forest(hits: Vec<c10::Hit>) -> FakeGateway {
    let fake = FakeGateway::start(vec![]);
    fake.respond(
        "POST /v1/forest_search",
        Box::new(|_| {
            let error = c10::Error {
                code: c10::ErrorCode::UnknownMethod,
                message: "no such method or path".into(),
            };
            json_bytes(404, &serde_json::to_vec(&error).unwrap())
        }),
    );
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
    fake
}
