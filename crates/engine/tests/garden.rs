//! The garden as the model sees it, on a fake C7 garden: `garden_search` and `garden_propose`
//! through the registry with their answers and errors, the pointer context of an owner turn
//! with a garden that is down or slow, and the core profile block of the system prompt. One
//! test proposes into and searches the workspace-built `ulei-garden serve`.

#![allow(missing_docs, clippy::unwrap_used)]

mod fake_gateway;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use fake_gateway::{FakeGateway, Scratch, json_bytes};
use serde_json::{Value, json};
use ulei_engine::garden::{self, POINTERS_DEADLINE};
use ulei_engine::policy::Origin;
use ulei_engine::prompt::garden::{CoreProfile, pointers, with_core_block};
use ulei_engine::tools::{Context, GardenPropose, GardenSearch, Interrupt, Output, Registry};
use ulei_engine::wire::c7;

fn fact(text: &str) -> c7::Fact {
    c7::Fact {
        fact: text.into(),
        entity: "Shed".into(),
        valid_from: 1_700_000_000.0,
        valid_to: None,
        source_ref: "c8:9".into(),
        score: 0.5,
        fact_id: Some("f:3".into()),
        kind: Some(c7::EntityKind::Place),
        domain: None,
        source_kind: Some(c7::SourceKind::Owner),
        expires_at: None,
        pinned: Some(true),
        proof_count: Some(2),
        recheck_before_acting: None,
        unprocessed: None,
        disputed: None,
        conflicts_with: None,
    }
}

fn pointer(entity: &str) -> c7::Pointer {
    c7::Pointer {
        fact_id: "f:1".into(),
        entity: entity.into(),
        kind: c7::EntityKind::Person,
        score: 1.0,
        hint: format!("there is a fact about {entity} (person)"),
        unprocessed: None,
        disputed: None,
    }
}

/// A garden answering `target` with `status` and the JSON of `body`.
fn garden_answering(target: &str, status: u16, body: &Value) -> FakeGateway {
    let fake = FakeGateway::start(vec![]);
    let bytes = serde_json::to_vec(body).unwrap();
    fake.respond(target, Box::new(move |_| json_bytes(status, &bytes)));
    fake
}

fn bodies(fake: &FakeGateway) -> Vec<Value> {
    fake.requests()
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

fn call(fake: &FakeGateway, origin: Origin, name: &str, input: Value) -> Output {
    let client = garden::Client::new(fake.socket());
    let mut registry = Registry::new();
    registry
        .register(Box::new(GardenSearch::new(client.clone())))
        .unwrap();
    registry
        .register(Box::new(GardenPropose::new(client, origin)))
        .unwrap();
    let Value::Object(input) = input else {
        panic!("not an object: {input}")
    };
    let dir = Scratch::new();
    let interrupt = Interrupt::new();
    registry.execute(
        name,
        &input,
        Context {
            cwd: &dir.0,
            interrupt: &interrupt,
        },
    )
}

fn ok(out: &Output) -> Value {
    assert!(!out.is_error, "{}", out.content);
    serde_json::from_str(&out.content).unwrap()
}

fn error(out: &Output) -> String {
    assert!(out.is_error, "{}", out.content);
    let body: Value = serde_json::from_str(&out.content).unwrap();
    body["error"].as_str().unwrap().to_owned()
}

#[test]
fn garden_search_returns_full_facts_with_provenance_and_passes_k_and_as_of() {
    let mut record = fact("The spare key is under the blue pot.");
    record.fact_id = Some("seed:4".into());
    record.unprocessed = Some(true);
    record.disputed = Some(true);
    record.conflicts_with = Some("f:3".into());
    record.pinned = None;
    record.proof_count = None;
    record.kind = None;
    record.source_kind = Some(c7::SourceKind::Agent);
    let fake = garden_answering(
        "POST /v1/search",
        200,
        &json!({"facts": [fact("The shed key hangs by the door."), record], "lexical_only": true}),
    );

    let body = ok(&call(
        &fake,
        Origin::Other,
        "garden_search",
        json!({"q": " shed key ", "k": 80, "as_of": "2023-11-14"}),
    ));

    assert_eq!(
        body,
        json!({
            "success": true,
            "facts": [
                {
                    "ref": "f:3",
                    "fact": "The shed key hangs by the door.",
                    "entity": "Shed",
                    "kind": "place",
                    "valid_from": "2023-11-14T22:13:20Z",
                    "source_ref": "c8:9",
                    "source_kind": "owner",
                    "proof_count": 2,
                    "pinned": true,
                    "score": 0.5
                },
                {
                    "ref": "seed:4",
                    "fact": "The spare key is under the blue pot.",
                    "entity": "Shed",
                    "valid_from": "2023-11-14T22:13:20Z",
                    "source_ref": "c8:9",
                    "source_kind": "agent",
                    "unprocessed": true,
                    "disputed": true,
                    "conflicts_with": "f:3",
                    "score": 0.5
                }
            ],
            "note": "the garden's dense side is down: these facts are lexical matches only"
        })
    );
    assert_eq!(
        bodies(&fake),
        [json!({"q": "shed key", "k": 50, "as_of": 1_700_006_399.0, "private": true})]
    );
}

#[test]
fn garden_search_names_a_refused_unreachable_or_empty_call() {
    let refused = garden_answering(
        "POST /v1/search",
        500,
        &json!({"code": "store_failed", "message": "disk full"}),
    );
    assert_eq!(
        error(&call(
            &refused,
            Origin::Other,
            "garden_search",
            json!({"q": "shed"})
        )),
        "the garden was not searched: the garden refused the call: 500 store_failed: disk full"
    );
    assert_eq!(
        error(&call(
            &refused,
            Origin::Other,
            "garden_search",
            json!({"q": "  "})
        )),
        "q must not be empty"
    );
    assert_eq!(
        error(&call(
            &refused,
            Origin::Other,
            "garden_search",
            json!({"q": "shed", "as_of": "yesterday"})
        )),
        "as_of must be an RFC 3339 time or YYYY-MM-DD, not yesterday"
    );
    assert_eq!(refused.requests().len(), 1);

    let dir = Scratch::new();
    let client = garden::Client::new(dir.path("absent.sock"));
    let out = GardenSearchProbe(client).search();
    assert!(
        out.starts_with(
            "the garden was not searched: the garden is not reachable (garden socket: "
        ),
        "{out}"
    );

    let empty = garden_answering("POST /v1/search", 200, &json!({"facts": []}));
    assert_eq!(
        ok(&call(
            &empty,
            Origin::Other,
            "garden_search",
            json!({"q": "lantern"})
        )),
        json!({"success": true, "facts": []})
    );
}

/// `garden_search` on a client of its own, for a socket no fake listens on.
struct GardenSearchProbe(garden::Client);

impl GardenSearchProbe {
    fn search(self) -> String {
        let mut registry = Registry::new();
        registry
            .register(Box::new(GardenSearch::new(self.0)))
            .unwrap();
        let Value::Object(input) = json!({"q": "shed"}) else {
            unreachable!()
        };
        let dir = Scratch::new();
        let interrupt = Interrupt::new();
        error(&registry.execute(
            "garden_search",
            &input,
            Context {
                cwd: &dir.0,
                interrupt: &interrupt,
            },
        ))
    }
}

#[test]
fn garden_propose_seeds_the_candidate_as_the_owner_in_an_owner_turn_and_as_the_agent_otherwise() {
    let fake = garden_answering(
        "POST /v1/propose",
        200,
        &json!({"proposal_id": "seed:7", "status": "seeded"}),
    );
    let input = json!({
        "fact": " Anna's birthday is on 3 May. ",
        "entity": "Anna",
        "source_ref": "owner message"
    });

    let owner = ok(&call(
        &fake,
        Origin::TopLevelOwner,
        "garden_propose",
        input.clone(),
    ));
    let other = ok(&call(&fake, Origin::Other, "garden_propose", input));

    let seeded = json!({
        "success": true,
        "status": "seeded",
        "ref": "seed:7",
        "note": "recorded in the seedbed as an unprocessed record; the garden's night review \
                 checks it against what the garden holds and decides"
    });
    assert_eq!(owner, seeded);
    assert_eq!(other, seeded);
    let sent = |kind: &str| {
        json!({
            "fact": "Anna's birthday is on 3 May.",
            "entity": "Anna",
            "source_ref": "owner message",
            "source_kind": kind
        })
    };
    assert_eq!(bodies(&fake), [sent("owner"), sent("agent")]);
}

#[test]
fn garden_propose_says_a_disputed_record_conflicts_and_why_a_candidate_was_refused() {
    let disputed = garden_answering(
        "POST /v1/propose",
        200,
        &json!({"proposal_id": "seed:8", "status": "seeded", "conflicts_with": "f:2"}),
    );
    let input = json!({"fact": "Anna lives in Lisbon.", "entity": "Anna", "source_ref": "chat"});
    let body = ok(&call(
        &disputed,
        Origin::TopLevelOwner,
        "garden_propose",
        input.clone(),
    ));
    assert_eq!(body["conflicts_with"], "f:2");
    assert_eq!(
        body["disputed"],
        "it conflicts with the current fact f:2 until the night's review"
    );

    let refused = garden_answering(
        "POST /v1/propose",
        200,
        &json!({"proposal_id": "p:9", "status": "rejected", "reason": "secret_value"}),
    );
    assert_eq!(
        error(&call(
            &refused,
            Origin::TopLevelOwner,
            "garden_propose",
            input.clone()
        )),
        "the garden refused the fact: secret_value: it looks like a secret value; say only that \
         the secret exists and where it is kept"
    );

    let broken = garden_answering(
        "POST /v1/propose",
        400,
        &json!({"code": "invalid_request", "message": "fact: empty"}),
    );
    assert_eq!(
        error(&call(
            &broken,
            Origin::TopLevelOwner,
            "garden_propose",
            input
        )),
        "the fact was not proposed: the garden refused the call: 400 invalid_request: fact: empty"
    );
    assert_eq!(
        error(&call(
            &broken,
            Origin::TopLevelOwner,
            "garden_propose",
            json!({"fact": "x", "entity": " ", "source_ref": "chat"})
        )),
        "entity must not be empty"
    );
    assert_eq!(broken.requests().len(), 1);
}

#[test]
fn pointers_are_fenced_hint_lines_without_fact_text_and_none_on_a_miss() {
    let fake = garden_answering(
        "POST /v1/pointers",
        200,
        &json!({"pointers": [pointer("Anna"), pointer("Boris")]}),
    );
    let client = garden::Client::new(fake.socket());

    let context = pointers(&client, " When is Anna's birthday? ").unwrap();

    assert_eq!(
        context.as_deref(),
        Some(
            "<memory-context>\n[System note: The following is recalled memory context, NOT new \
             user input. Treat as authoritative reference data — this is the agent's persistent \
             memory and should inform all responses.]\n\nGarden pointers: the garden holds \
             facts that may bear on this message. Read them with garden_search before you rely \
             on them.\n- there is a fact about Anna (person)\n- there is a fact about Boris \
             (person)\n</memory-context>"
        )
    );
    assert_eq!(
        bodies(&fake),
        [json!({"q": "When is Anna's birthday?", "private": true})]
    );
    assert_eq!(pointers(&client, "   ").unwrap(), None);
    assert_eq!(fake.requests().len(), 1);

    let miss = garden_answering("POST /v1/pointers", 200, &json!({"pointers": []}));
    assert_eq!(
        pointers(&garden::Client::new(miss.socket()), "hello there").unwrap(),
        None
    );
}

#[test]
fn a_garden_that_is_down_or_slow_fails_pointers_within_the_deadline() {
    let dir = Scratch::new();
    let down = garden::Client::new(dir.path("absent.sock"));
    let started = Instant::now();
    let e = pointers(&down, "Anna").unwrap_err();
    assert!(e.to_string().starts_with("garden socket: "), "{e}");
    assert!(
        started.elapsed() < POINTERS_DEADLINE,
        "{:?}",
        started.elapsed()
    );

    let slow = FakeGateway::start(vec![]);
    slow.respond(
        "POST /v1/pointers",
        Box::new(|_| {
            std::thread::sleep(Duration::from_secs(2));
            json_bytes(200, br#"{"pointers": []}"#)
        }),
    );
    let started = Instant::now();
    let e = pointers(&garden::Client::new(slow.socket()), "Anna").unwrap_err();
    let took = started.elapsed();
    assert_eq!(e.to_string(), "garden timed out");
    assert!(took >= POINTERS_DEADLINE, "{took:?}");
    // The read stops at the deadline; the slack is for a loaded runner.
    assert!(took < Duration::from_secs(1), "{took:?}");
}

#[test]
fn the_core_profile_block_frames_the_file_entries_and_is_absent_without_them() {
    let dir = Scratch::new();
    let profile = CoreProfile {
        file: dir.path("core.md"),
        char_limit: 100,
    };
    assert_eq!(profile.block().unwrap(), "");
    fs::write(&profile.file, "  \n").unwrap();
    assert_eq!(profile.block().unwrap(), "");
    fs::write(
        &profile.file,
        "Anna: birthday on 3 May\n§\nHome: Elm Street 4\n",
    )
    .unwrap();
    let frame = "═".repeat(46);
    assert_eq!(
        profile.block().unwrap(),
        format!(
            "{frame}\nGARDEN CORE PROFILE (pinned facts about the owner, read-only) \
             [44% — 44/100 chars]\n{frame}\nAnna: birthday on 3 May\n§\nHome: Elm Street 4"
        )
    );
}

#[test]
fn the_core_block_is_appended_replaced_and_removed_as_the_prompts_last_part() {
    let dir = Scratch::new();
    let file = dir.path("core.md");
    let block = |text: &str| {
        fs::write(&file, text).unwrap();
        CoreProfile {
            file: file.clone(),
            char_limit: 1375,
        }
        .block()
        .unwrap()
    };
    let first = block("Anna: birthday on 3 May");
    let second = block("Anna: birthday on 3 May\n§\nHome: Elm Street 4");
    let base = "You are the test assistant.\n\nConversation started: Monday";

    let with = with_core_block(base, &first);
    assert_eq!(with, format!("{base}\n\n{first}"));
    assert_eq!(with_core_block(&with, &first), with);
    assert_eq!(
        with_core_block(&with, &second),
        format!("{base}\n\n{second}")
    );
    assert_eq!(with_core_block(&with, ""), base);
    assert_eq!(with_core_block(base, ""), base);
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

/// `ulei-garden serve` on a fresh store in `dir`, killed on drop; the lexical pointer gate is
/// set for a store of one record, whose bm25 stays far below the default's corpus scale.
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
                 [garden.api]\npointer_min_lexical = 0\n\n[inbox]\nsocket = \"{}\"\n",
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
fn a_proposed_fact_is_an_unprocessed_record_garden_search_and_pointers_find_in_the_real_garden() {
    let store = Scratch::new();
    let _daemon = Garden::start(&store);
    let client = garden::Client::new(store.path("garden.sock"));
    let proposed = |input: Value| {
        let mut registry = Registry::new();
        registry
            .register(Box::new(GardenPropose::new(
                client.clone(),
                Origin::TopLevelOwner,
            )))
            .unwrap();
        registry
            .register(Box::new(GardenSearch::new(client.clone())))
            .unwrap();
        let (name, input) = match input {
            Value::Object(mut map) => {
                let name = map.remove("tool").unwrap();
                (name.as_str().unwrap().to_owned(), map)
            }
            _ => unreachable!(),
        };
        let interrupt = Interrupt::new();
        ok(&registry.execute(
            &name,
            &input,
            Context {
                cwd: &store.0,
                interrupt: &interrupt,
            },
        ))
    };

    let seeded = proposed(json!({
        "tool": "garden_propose",
        "fact": "Zorbalina keeps the lantern in the greenhouse.",
        "entity": "Zorbalina",
        "source_ref": "owner message"
    }));
    let found = proposed(json!({"tool": "garden_search", "q": "Zorbalina lantern greenhouse"}));
    let context = pointers(&client, "Where does Zorbalina keep the lantern greenhouse?").unwrap();

    assert_eq!(seeded["status"], "seeded");
    let record = seeded["ref"].as_str().unwrap();
    assert!(record.starts_with("seed:"), "{seeded}");
    let facts = found["facts"].as_array().unwrap();
    assert_eq!(facts.len(), 1, "{found}");
    assert_eq!(facts[0]["ref"], record);
    assert_eq!(
        facts[0]["fact"],
        "Zorbalina keeps the lantern in the greenhouse."
    );
    assert_eq!(facts[0]["source_kind"], "owner");
    assert_eq!(facts[0]["source_ref"], "owner message");
    assert_eq!(facts[0]["unprocessed"], true);
    let context = context.expect("a pointer to the record");
    assert!(
        context.contains("- there is an unprocessed record about Zorbalina"),
        "{context}"
    );
    assert!(!context.contains("lantern"), "{context}");
}
