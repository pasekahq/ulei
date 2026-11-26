//! The night gardener (ADR-0078) through `night::run` on a scripted gateway, a fake C7 garden
//! that keeps a seedbed and a forget queue in memory, and a fake C10 archive of synthetic
//! history: the seedbed into `seedbed_done`, a malformed answer leaving its batch, a batch whose
//! answer hits `max_tokens` split in halves down to one record, the forget
//! queue cleared, three sessions into candidates and forest records with a rerun sending nothing
//! and a rerun of a session replacing its record, the
//! spend limit stopping a run that the next run resumes, and a dry run sending and writing
//! nothing.

#![allow(missing_docs, clippy::unwrap_used)]

mod fake_gateway;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fake_gateway::{
    Answer, FakeGateway, Request, STREAM_HEAD, Scratch, chunk, json_bytes, line, route, stop, text,
    usage_of,
};
use serde_json::{Value, json};
use ulei_engine::gateway::Client;
use ulei_engine::night::extract::Gateway;
use ulei_engine::night::spend::Prices;
use ulei_engine::night::{self, Options, Settings, Stage, Stop};
use ulei_engine::wire::c4g::StopReason;
use ulei_engine::wire::{c7, c10};

/// One seedbed record of the fake garden.
#[derive(Clone)]
struct Seed {
    seq: u64,
    candidate: c7::ProposeRequest,
    done: Option<Vec<c7::ProposeRequest>>,
}

#[derive(Default)]
struct GardenState {
    seeds: Vec<Seed>,
    /// Forget rows: id, fact id, taken.
    queue: Vec<(u64, String, bool)>,
    /// Facts the forget queue closed.
    closed: Vec<String>,
    /// Every `propose` request.
    proposed: Vec<c7::ProposeRequest>,
}

fn candidate(fact: &str, source_ref: &str) -> c7::ProposeRequest {
    serde_json::from_value(json!({"fact": fact, "source_ref": source_ref, "source_kind": "owner"}))
        .unwrap()
}

/// A garden whose edge accepts a candidate of confidence 0.6 or more and queues the others.
#[allow(
    clippy::too_many_lines,
    reason = "one responder per C7 method of the fake"
)]
fn fake_garden(state: &Arc<Mutex<GardenState>>) -> FakeGateway {
    let fake = FakeGateway::start(vec![]);
    let edge = |c: &c7::ProposeRequest, n: usize| {
        let accepted = c.confidence.unwrap_or(1.0) >= 0.6;
        json!({"proposal_id": format!("p-{n}"),
            "status": if accepted { "accepted" } else { "queued" },
            "reason": if accepted { Value::Null } else { "low_confidence".into() }})
    };
    let s = Arc::clone(state);
    fake.respond(
        "POST /v1/seedbed_take",
        Box::new(move |request| {
            let take: c7::SeedbedTakeRequest = serde_json::from_slice(&request.body).unwrap();
            let after = take
                .after
                .map_or(0, |r| r["seed:".len()..].parse::<u64>().unwrap());
            let state = s.lock().unwrap();
            let records: Vec<Value> = state
                .seeds
                .iter()
                .filter(|r| r.done.is_none() && r.seq > after)
                .take(usize::try_from(take.limit.unwrap_or(50)).unwrap())
                .map(|r| {
                    json!({"ref": format!("seed:{}", r.seq), "candidate": r.candidate,
                        "written_at": 1_700_000_000})
                })
                .collect();
            json_bytes(
                200,
                &serde_json::to_vec(&json!({"records": records})).unwrap(),
            )
        }),
    );
    let s = Arc::clone(state);
    fake.respond(
        "POST /v1/seedbed_done",
        Box::new(move |request| {
            let done: c7::SeedbedDoneRequest = serde_json::from_slice(&request.body).unwrap();
            let mut state = s.lock().unwrap();
            let mut items = Vec::new();
            for item in done.items {
                let seq: u64 = item.r#ref["seed:".len()..].parse().unwrap();
                let seed = state.seeds.iter_mut().find(|r| r.seq == seq).unwrap();
                let candidates = item
                    .candidates
                    .clone()
                    .unwrap_or_else(|| vec![seed.candidate.clone()]);
                let outcomes: Vec<Value> = candidates
                    .iter()
                    .enumerate()
                    .map(|(n, c)| edge(c, n))
                    .collect();
                seed.done.get_or_insert(candidates);
                items.push(json!({"ref": item.r#ref, "outcomes": outcomes}));
            }
            json_bytes(200, &serde_json::to_vec(&json!({"items": items})).unwrap())
        }),
    );
    let s = Arc::clone(state);
    fake.respond(
        "POST /v1/propose",
        Box::new(move |request| {
            let propose: c7::ProposeRequest = serde_json::from_slice(&request.body).unwrap();
            let mut state = s.lock().unwrap();
            state.proposed.push(propose.clone());
            let existing = state
                .seeds
                .iter()
                .find(|r| r.candidate == propose)
                .map(|r| r.seq);
            let seq = existing.unwrap_or_else(|| {
                let seq = state.seeds.len() as u64 + 1;
                state.seeds.push(Seed {
                    seq,
                    candidate: propose,
                    done: None,
                });
                seq
            });
            let answer = json!({"proposal_id": format!("seed:{seq}"), "status": "seeded"});
            json_bytes(200, &serde_json::to_vec(&answer).unwrap())
        }),
    );
    let s = Arc::clone(state);
    fake.respond(
        "POST /v1/forget_take",
        Box::new(move |request| {
            let take: c7::ForgetTakeRequest = serde_json::from_slice(&request.body).unwrap();
            let state = s.lock().unwrap();
            let rows: Vec<Value> = state
                .queue
                .iter()
                .filter(|(id, _, taken)| !taken && *id > take.after.unwrap_or(0))
                .map(|(id, fact, _)| {
                    json!({"id": id, "fact_id": fact, "ref": "seed:1", "marked_at": 1,
                        "reason": "wrong", "queued_at": 1})
                })
                .collect();
            json_bytes(200, &serde_json::to_vec(&json!({"rows": rows})).unwrap())
        }),
    );
    let s = Arc::clone(state);
    fake.respond(
        "POST /v1/forget_done",
        Box::new(move |request| {
            let done: c7::ForgetDoneRequest = serde_json::from_slice(&request.body).unwrap();
            let mut state = s.lock().unwrap();
            let mut items = Vec::new();
            for id in done.ids {
                let row = state.queue.iter_mut().find(|r| r.0 == id).unwrap();
                let closed = !row.2;
                row.2 = true;
                let fact = row.1.clone();
                if closed {
                    state.closed.push(fact.clone());
                }
                items.push(json!({"id": id, "fact_id": fact, "closed": closed}));
            }
            json_bytes(200, &serde_json::to_vec(&json!({"items": items})).unwrap())
        }),
    );
    fake
}

/// An archive serving `entries` of the source `old-runtime` by `seq`, whose forest keeps every
/// `forest_put` and replaces a current record of the same session sharing a link, as the archive
/// does.
fn fake_archive(entries: Vec<c10::Listed>) -> FakeGateway {
    let fake = FakeGateway::start(vec![]);
    let forest: Mutex<Vec<(c10::ForestPutRequest, bool)>> = Mutex::default();
    fake.respond(
        "POST /v1/forest_put",
        Box::new(move |request| {
            let put: c10::ForestPutRequest = serde_json::from_slice(&request.body).unwrap();
            let mut forest = forest.lock().unwrap();
            let mut replaced = Vec::new();
            for (n, (old, current)) in forest.iter_mut().enumerate() {
                let shares = old.links.iter().any(|l| put.links.contains(l));
                if *current && old.session == put.session && shares {
                    *current = false;
                    replaced.push(n as u64 + 1);
                }
            }
            forest.push((put, true));
            let answer = c10::ForestPutResponse {
                id: forest.len() as u64,
                status: c10::PutStatus::Stored,
                replaced: (!replaced.is_empty()).then_some(replaced),
            };
            json_bytes(200, &serde_json::to_vec(&answer).unwrap())
        }),
    );
    fake.respond(
        "POST /v1/list",
        Box::new(move |request| {
            let list: c10::ListRequest = serde_json::from_slice(&request.body).unwrap();
            let after = list.after.unwrap_or(0);
            let limit = usize::try_from(list.limit.unwrap_or(100)).unwrap();
            let rest: Vec<&c10::Listed> = entries
                .iter()
                .filter(|e| list.source == "old-runtime" && e.seq > after)
                .collect();
            let page: Vec<&c10::Listed> = rest.iter().take(limit).copied().collect();
            let answer = json!({"entries": page, "more": rest.len() > limit});
            json_bytes(200, &serde_json::to_vec(&answer).unwrap())
        }),
    );
    fake
}

fn entry(seq: u64, external_id: &str, who: &str, at: f64, text: &str) -> c10::Listed {
    c10::Listed {
        seq,
        id: format!("{seq:064x}"),
        external_id: external_id.to_owned(),
        who: Some(who.to_owned()),
        at: Some(at),
        kind: "application/json".to_owned(),
        put_at: 1.0,
        text: text.to_owned(),
        text_truncated: None,
    }
}

/// Three sessions of two kept messages each, a session row and a tool message.
fn history() -> Vec<c10::Listed> {
    let mut entries = Vec::new();
    let mut seq = 0;
    for s in 1..=3 {
        seq += 1;
        entries.push(entry(seq, &format!("s-{s}"), "system", 0.0, "session row"));
        for (m, who) in [(1, "user"), (2, "tool"), (3, "assistant")] {
            seq += 1;
            let at = f64::from(1_700_000_000 + s * 1_000 + m);
            let text = format!("session {s} message {m}: Ada Example lives in Town {s}.");
            entries.push(entry(seq, &format!("s-{s}/m-{m}"), who, at, &text));
        }
    }
    entries
}

/// A turn answering `answer` with `input` and `output` tokens.
fn turn(answer: &str, input: u64, output: u64) -> Answer {
    Answer::Events(vec![
        route(),
        text(answer),
        usage_of(input, output),
        stop(StopReason::End),
    ])
}

fn history_answer(n: u32) -> String {
    json!({"summary": format!("Session {n}: Ada's town."),
        "candidates": [{"entity": "Ada Example", "kind": "person",
            "fact": format!("Ada Example lives in Town {n}."), "message": 1,
            "attribute": "home_city", "confidence": 0.9}]})
    .to_string()
}

struct Rig {
    gateway: FakeGateway,
    garden: FakeGateway,
    archive: FakeGateway,
    state: Arc<Mutex<GardenState>>,
    dir: Scratch,
}

impl Rig {
    fn new(script: Vec<Answer>, entries: Vec<c10::Listed>) -> Self {
        let state = Arc::new(Mutex::new(GardenState::default()));
        Self {
            gateway: FakeGateway::start(script),
            garden: fake_garden(&state),
            archive: fake_archive(entries),
            state,
            dir: Scratch::new(),
        }
    }

    fn settings(&self) -> Settings {
        Settings {
            gateway: Some(self.gateway.socket().to_owned()),
            garden: Some(self.garden.socket().to_owned()),
            archive: Some(self.archive.socket().to_owned()),
            state: Some(self.dir.path("night")),
            route: "small".to_owned(),
            max_usd: 1.0,
            prices: Prices::DEFAULT,
            max_prompt_tokens: 60_000,
            max_tokens: 8_192,
            timeout: Duration::from_secs(10),
            seed_batch: 50,
            sources: vec!["old-runtime".to_owned()],
            separator: "/".to_owned(),
            roles: vec!["user".to_owned(), "assistant".to_owned()],
            message_chars: 3_000,
        }
    }

    fn run(
        &self,
        settings: &Settings,
        stages: &[Stage],
        limit: Option<u64>,
    ) -> (Stop, night::Totals, Option<PathBuf>) {
        let options = Options {
            dry_run: false,
            stages: stages.to_vec(),
            limit_sessions: limit,
        };
        let mut model = Gateway::new(
            Client::new(
                self.gateway.socket(),
                Duration::from_secs(5),
                Duration::from_secs(10),
            ),
            settings.route.clone(),
            settings.max_tokens,
            settings.timeout,
            "test".to_owned(),
        );
        let mut out = Vec::new();
        night::run(settings, &options, Some(&mut model), &mut out)
    }

    fn turns(&self) -> usize {
        self.gateway
            .requests()
            .iter()
            .filter(|r| r.target() == "POST /v1/model/turn")
            .count()
    }
}

/// The `forest_put` requests a fake archive received, in order.
fn forest_puts(archive: &FakeGateway) -> Vec<c10::ForestPutRequest> {
    archive
        .requests()
        .iter()
        .filter(|r| r.target() == "POST /v1/forest_put")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

fn report(path: &PathBuf) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn seed(state: &Arc<Mutex<GardenState>>, text: &str, source_ref: &str) {
    let mut state = state.lock().unwrap();
    let seq = state.seeds.len() as u64 + 1;
    state.seeds.push(Seed {
        seq,
        candidate: candidate(text, source_ref),
        done: None,
    });
}

#[test]
fn seedbed_records_go_through_the_model_to_seedbed_done_with_their_evidence() {
    let answer = json!({"records": [
        {"ref": "seed:1", "candidates": [{"entity": "Ada Example", "kind": "person",
            "fact": "Ada Example lives at 9 Sample Road.", "attribute": "address",
            "confidence": 0.9}]},
        {"ref": "seed:2", "candidates": [{"entity": "Cafe Moss", "kind": "place",
            "fact": "Cafe Moss closes at 18:00.", "confidence": 0.3}]},
        {"ref": "seed:3", "candidates": []}]})
    .to_string();
    let rig = Rig::new(vec![turn(&answer, 2_000, 300)], Vec::new());
    seed(&rig.state, "Ada lives at 9 Sample Road now.", "c8:11");
    seed(&rig.state, "Cafe Moss closes early.", "c8:12");
    seed(&rig.state, "hello", "c8:13");
    {
        let mut state = rig.state.lock().unwrap();
        let mut secret = candidate("The router password is in the vault.", "c8:14");
        secret.secret_ref = Some("vault:home/router".to_owned());
        secret.entity = Some("Router".to_owned());
        state.seeds.push(Seed {
            seq: 4,
            candidate: secret,
            done: None,
        });
    }
    let (stop, totals, path) = rig.run(&rig.settings(), &[Stage::Seedbed], None);
    assert_eq!(stop, Stop::Done);
    assert_eq!(rig.turns(), 1);
    let turn: Value = serde_json::from_slice(&rig.gateway.requests()[0].body).unwrap();
    let prompt = turn["messages"][0]["content"][0]["text"].as_str().unwrap();
    assert!(prompt.contains("seed:1") && prompt.contains("seed:3"));
    assert!(
        !prompt.contains("vault"),
        "a secret pointer never reaches the model"
    );
    assert!(
        turn["system"]
            .as_str()
            .unwrap()
            .contains("refuses secret values")
    );
    let state = rig.state.lock().unwrap();
    let first = &state.seeds[0].done.as_ref().unwrap()[0];
    assert_eq!(first.source_ref, "c8:11");
    assert_eq!(first.source_kind, Some(c7::SourceKind::Owner));
    assert_eq!(first.entity.as_deref(), Some("Ada Example"));
    assert_eq!(first.attribute.as_deref(), Some("address"));
    assert_eq!(state.seeds[2].done.as_ref().unwrap().len(), 0);
    assert_eq!(
        state.seeds[3].done.as_ref().unwrap()[0]
            .secret_ref
            .as_deref(),
        Some("vault:home/router")
    );
    assert_eq!(
        (
            totals.taken,
            totals.proposed,
            totals.accepted,
            totals.digest
        ),
        (4, 3, 2, 1)
    );
    assert_eq!((totals.tokens_in, totals.tokens_out), (2_000, 300));
    assert!((totals.usd - (2_000.0 * 1.0 + 300.0 * 5.0) / 1e6).abs() < 1e-12);
    let lines = report(path.as_ref().unwrap());
    assert_eq!(lines.last().unwrap()["type"], "total");
    assert_eq!(lines.last().unwrap()["stop"], "done");
}

#[test]
fn a_malformed_answer_is_asked_once_more_then_its_batch_waits_for_the_next_night() {
    let rig = Rig::new(
        vec![
            turn("not json", 100, 10),
            turn("{\"records\": []}", 100, 10),
            turn(
                &json!({"records": [{"ref": "seed:1", "candidates": []}]}).to_string(),
                100,
                10,
            ),
        ],
        Vec::new(),
    );
    seed(&rig.state, "Ada likes tea.", "c8:1");
    let (stop, totals, _) = rig.run(&rig.settings(), &[Stage::Seedbed], None);
    assert_eq!(stop, Stop::Done);
    assert_eq!(rig.turns(), 2);
    assert_eq!((totals.malformed, totals.left, totals.taken), (1, 1, 0));
    assert!(rig.state.lock().unwrap().seeds[0].done.is_none());
    let (stop, totals, _) = rig.run(&rig.settings(), &[Stage::Seedbed], None);
    assert_eq!(stop, Stop::Done);
    assert_eq!(rig.turns(), 3);
    assert_eq!((totals.malformed, totals.taken), (0, 1));
    assert!(rig.state.lock().unwrap().seeds[0].done.is_some());
}

/// The seedbed refs of a turn request's prompt, in order.
fn prompt_refs(request: &Request) -> Vec<String> {
    let turn: Value = serde_json::from_slice(&request.body).unwrap();
    let prompt = turn["messages"][0]["content"][0]["text"].as_str().unwrap();
    prompt
        .split("\"ref\":\"")
        .skip(1)
        .map(|rest| rest.split('"').next().unwrap().to_owned())
        .collect()
}

/// Answers every turn as a model whose answer fits `max_tokens` only when `fits` holds for the
/// refs of its prompt: a `max_tokens` stop with a cut answer otherwise, else one candidate per
/// record. Each call reports `input` input tokens and 10 output tokens.
fn overflow_model(gateway: &FakeGateway, input: u64, fits: fn(&[String]) -> bool) {
    gateway.respond(
        "POST /v1/model/turn",
        Box::new(move |request| {
            let refs = prompt_refs(request);
            let events = if fits(&refs) {
                let records: Vec<Value> = refs
                    .iter()
                    .map(|r| {
                        json!({"ref": r, "candidates": [{"entity": "Ada Example",
                            "kind": "person", "fact": format!("Ada Example noted {r}."),
                            "confidence": 0.9}]})
                    })
                    .collect();
                vec![
                    route(),
                    text(&json!({ "records": records }).to_string()),
                    usage_of(input, 10),
                    stop(StopReason::End),
                ]
            } else {
                vec![
                    route(),
                    text("{\"records\": [{\"ref\": "),
                    usage_of(input, 10),
                    stop(StopReason::MaxTokens),
                ]
            };
            let mut bytes = STREAM_HEAD.to_vec();
            for event in &events {
                bytes.extend(chunk(&line(event)));
            }
            bytes.extend_from_slice(b"0\r\n\r\n");
            bytes
        }),
    );
}

fn seed_n(rig: &Rig, n: u64) {
    for i in 1..=n {
        seed(
            &rig.state,
            &format!("Note {i} about Ada."),
            &format!("c8:{i}"),
        );
    }
}

/// The `records` of each turn the gateway received, in order.
fn turn_sizes(gateway: &FakeGateway) -> Vec<usize> {
    gateway
        .requests()
        .iter()
        .filter(|r| r.target() == "POST /v1/model/turn")
        .map(|r| prompt_refs(r).len())
        .collect()
}

#[test]
fn a_batch_whose_answer_hits_max_tokens_is_split_in_halves_until_each_fits() {
    let rig = Rig::new(Vec::new(), Vec::new());
    overflow_model(&rig.gateway, 100, |refs| refs.len() <= 2);
    seed_n(&rig, 7);
    let (stop, totals, path) = rig.run(&rig.settings(), &[Stage::Seedbed], None);
    assert_eq!(stop, Stop::Done);
    // 7 overflows, then 3 (overflows: 1, 2) and 4 (overflows: 2, 2); never the same prompt twice.
    assert_eq!(turn_sizes(&rig.gateway), vec![7, 3, 1, 2, 4, 2, 2]);
    assert_eq!(
        (totals.taken, totals.left, totals.split, totals.malformed),
        (7, 0, 3, 0)
    );
    assert_eq!((totals.proposed, totals.accepted), (7, 7));
    let state = rig.state.lock().unwrap();
    for seed in &state.seeds {
        let done = seed.done.as_ref().unwrap();
        assert_eq!(done.len(), 1);
        assert_eq!(
            done[0].fact,
            format!("Ada Example noted seed:{}.", seed.seq)
        );
    }
    let lines = report(path.as_ref().unwrap());
    let outcomes: Vec<(&str, u64)> = lines
        .iter()
        .filter(|l| l["type"] == "seedbed")
        .map(|l| {
            (
                l["outcome"].as_str().unwrap(),
                l["records"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        outcomes,
        vec![
            ("split", 7),
            ("split", 3),
            ("done", 1),
            ("done", 2),
            ("split", 4),
            ("done", 2),
            ("done", 2)
        ]
    );
    assert_eq!(lines[0]["why"], "the answer hit max_tokens");
    assert_eq!(lines[0]["attempts"], 1);
    assert_eq!(lines.last().unwrap()["split"], 3);
}

#[test]
fn a_single_record_whose_answer_always_hits_max_tokens_is_left() {
    let rig = Rig::new(Vec::new(), Vec::new());
    overflow_model(&rig.gateway, 100, |refs| {
        refs.len() <= 2 && !refs.iter().any(|r| r == "seed:2")
    });
    seed_n(&rig, 3);
    let (stop, totals, path) = rig.run(&rig.settings(), &[Stage::Seedbed], None);
    assert_eq!(stop, Stop::Done);
    // 3 overflows, then 1 and 2 (with seed:2: overflows, then seed:2 alone once and seed:3).
    assert_eq!(turn_sizes(&rig.gateway), vec![3, 1, 2, 1, 1]);
    assert_eq!(
        (totals.taken, totals.left, totals.split, totals.malformed),
        (2, 1, 2, 1)
    );
    let state = rig.state.lock().unwrap();
    assert!(state.seeds[0].done.is_some());
    assert!(state.seeds[1].done.is_none());
    assert!(state.seeds[2].done.is_some());
    let left: Vec<Value> = report(path.as_ref().unwrap())
        .into_iter()
        .filter(|l| l["outcome"] == "left")
        .collect();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0]["first"], "seed:2");
    assert_eq!(left[0]["why"], "the answer hit max_tokens");
    assert_eq!(left[0]["attempts"], 1);
    drop(state);
    // The next night sends the left record again, alone.
    let (stop, totals, _) = rig.run(&rig.settings(), &[Stage::Seedbed], None);
    assert_eq!(stop, Stop::Done);
    assert_eq!((totals.taken, totals.left), (0, 1));
    assert_eq!(turn_sizes(&rig.gateway).last(), Some(&1));
}

#[test]
fn the_spend_limit_stops_a_split_between_its_halves_and_the_next_run_resumes() {
    let rig = Rig::new(Vec::new(), Vec::new());
    // Each call costs 0.1 USD of input: the whole batch and its first half fit under 0.15, the
    // second half does not start.
    overflow_model(&rig.gateway, 100_000, |refs| refs.len() <= 2);
    seed_n(&rig, 4);
    let mut settings = rig.settings();
    settings.max_usd = 0.15;
    let (stop, totals, path) = rig.run(&settings, &[Stage::Seedbed], None);
    assert_eq!(stop, Stop::SpendLimit);
    assert_eq!(turn_sizes(&rig.gateway), vec![4, 2]);
    assert_eq!((totals.taken, totals.left, totals.split), (2, 0, 1));
    {
        let state = rig.state.lock().unwrap();
        let done: Vec<bool> = state.seeds.iter().map(|s| s.done.is_some()).collect();
        assert_eq!(done, vec![true, true, false, false]);
    }
    assert_eq!(
        report(path.as_ref().unwrap()).last().unwrap()["stop"],
        "spend_limit"
    );
    let (stop, totals, _) = rig.run(&rig.settings(), &[Stage::Seedbed], None);
    assert_eq!(stop, Stop::Done);
    assert_eq!(turn_sizes(&rig.gateway), vec![4, 2, 2]);
    assert_eq!((totals.taken, totals.left), (2, 0));
    assert!(
        rig.state
            .lock()
            .unwrap()
            .seeds
            .iter()
            .all(|s| s.done.is_some())
    );
}

#[test]
fn the_forget_queue_is_cleared_without_a_model() {
    let rig = Rig::new(Vec::new(), Vec::new());
    rig.state.lock().unwrap().queue = vec![
        (1, "f-1".to_owned(), false),
        (2, "f-2".to_owned(), false),
        (3, "f-3".to_owned(), true),
    ];
    let (stop, totals, _) = rig.run(&rig.settings(), &[Stage::Forget], None);
    assert_eq!(stop, Stop::Done);
    assert_eq!((totals.forget_rows, totals.cleared), (2, 2));
    assert_eq!(rig.state.lock().unwrap().closed, ["f-1", "f-2"]);
    assert_eq!(rig.turns(), 0);
}

#[test]
fn three_sessions_become_candidates_and_summaries_and_a_rerun_sends_nothing() {
    let rig = Rig::new(
        (1..=3).map(|n| turn(&history_answer(n), 500, 80)).collect(),
        history(),
    );
    let (stop, totals, path) = rig.run(&rig.settings(), &[Stage::History], None);
    assert_eq!(stop, Stop::Done);
    assert_eq!(rig.turns(), 3);
    assert_eq!((totals.sessions, totals.messages), (3, 6));
    let requests = rig.gateway.requests();
    let first: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let transcript = first["messages"][0]["content"][0]["text"].as_str().unwrap();
    assert!(transcript.contains("[1] 2023-11-14 22:30 user: session 1 message 1"));
    assert!(transcript.contains("[2] 2023-11-14 22:30 assistant: session 1 message 3"));
    assert!(!transcript.contains("message 2"), "tool output is left out");
    let state = rig.state.lock().unwrap();
    assert_eq!(state.proposed.len(), 3);
    assert_eq!(state.proposed[0].source_ref, format!("archive:{:064x}", 2));
    assert_eq!(state.proposed[0].source_kind, Some(c7::SourceKind::Raw));
    assert_eq!(state.proposed[0].observed_at, Some(1_700_001_001.0));
    assert!(
        state.seeds.iter().all(|s| s.done.is_some()),
        "the edge decided them"
    );
    drop(state);
    assert_eq!(totals.accepted, 3);
    let sessions: Vec<Value> = report(path.as_ref().unwrap())
        .into_iter()
        .filter(|l| l["type"] == "session")
        .collect();
    assert_eq!(sessions.len(), 3);
    assert_eq!(sessions[0]["forest"], 1);
    assert_eq!(sessions[0]["forest_status"], "stored");
    assert_eq!(sessions[0]["from"], "2023-11-14T22:30:01Z");
    assert_eq!(sessions[0]["to"], "2023-11-14T22:30:03Z");
    let puts = forest_puts(&rig.archive);
    assert_eq!(puts.len(), 3);
    let run = path
        .as_ref()
        .unwrap()
        .file_stem()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        puts[0],
        c10::ForestPutRequest {
            source: "old-runtime".to_owned(),
            session: "s-1".to_owned(),
            text: "Session 1: Ada's town.".to_owned(),
            from: Some(1_700_001_001.0),
            to: Some(1_700_001_003.0),
            links: [2, 4]
                .iter()
                .map(|&seq| c10::ForestLink {
                    seq,
                    id: format!("{seq:064x}"),
                })
                .collect(),
            producer: c10::Producer {
                run,
                route: "small".to_owned(),
            },
        }
    );
    let (stop, totals, _) = rig.run(&rig.settings(), &[Stage::History], None);
    assert_eq!(stop, Stop::Done);
    assert_eq!(rig.turns(), 3, "a rerun sends nothing new");
    assert_eq!(totals.sessions, 0);
}

#[test]
fn the_spend_limit_stops_a_run_mid_way_and_the_next_run_resumes() {
    let rig = Rig::new(
        (1..=3)
            .map(|n| turn(&history_answer(n), 100_000, 10_000))
            .collect(),
        history(),
    );
    let mut settings = rig.settings();
    // One call costs 0.1 + 0.05 USD: the second call's estimated input still fits under 0.2,
    // the third does not start once 0.3 is spent.
    settings.max_usd = 0.2;
    let (stop, totals, path) = rig.run(&settings, &[Stage::History], None);
    assert_eq!(stop, Stop::SpendLimit);
    assert_eq!(rig.turns(), 2);
    assert_eq!(totals.sessions, 2);
    assert!(totals.usd >= 0.2);
    assert_eq!(
        report(path.as_ref().unwrap()).last().unwrap()["stop"],
        "spend_limit"
    );
    let (stop, totals, _) = rig.run(&rig.settings(), &[Stage::History], None);
    assert_eq!(stop, Stop::Done);
    assert_eq!(rig.turns(), 3);
    assert_eq!(totals.sessions, 1);
    let last: Value = serde_json::from_slice(&rig.gateway.requests()[2].body).unwrap();
    assert!(
        last["messages"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Session: s-3")
    );
}

#[test]
fn limit_sessions_ends_the_history_after_n_sessions() {
    let rig = Rig::new(
        (1..=3).map(|n| turn(&history_answer(n), 100, 10)).collect(),
        history(),
    );
    let (stop, totals, _) = rig.run(&rig.settings(), &[Stage::History], Some(1));
    assert_eq!(stop, Stop::SessionLimit);
    assert_eq!(totals.sessions, 1);
    assert_eq!(rig.turns(), 1);
}

#[test]
fn a_dry_run_sends_nothing_and_writes_nothing() {
    let rig = Rig::new(Vec::new(), history());
    seed(&rig.state, "Ada likes tea.", "c8:1");
    rig.state.lock().unwrap().queue = vec![(1, "f-1".to_owned(), false)];
    let settings = rig.settings();
    let options = Options {
        dry_run: true,
        stages: vec![Stage::Seedbed, Stage::Forget, Stage::History],
        limit_sessions: None,
    };
    let mut out = Vec::new();
    let (stop, totals, path) = night::run(&settings, &options, None, &mut out);
    assert_eq!(stop, Stop::Done);
    assert_eq!(path, None);
    assert_eq!(totals, night::Totals::default());
    assert!(!settings.state.as_ref().unwrap().exists());
    let lines: Vec<Value> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(
        (lines[0]["stage"].as_str(), lines[0]["items"].as_u64()),
        (Some("b"), Some(1))
    );
    assert_eq!(
        (lines[1]["stage"].as_str(), lines[1]["calls"].as_u64()),
        (Some("a"), Some(1))
    );
    assert_eq!(lines[2]["stage"], "c");
    assert_eq!(lines[2]["sessions"], 3);
    assert_eq!(lines[2]["messages"], 6);
    assert_eq!(lines[2]["calls"], 3);
    assert!(lines[2]["tokens_in"].as_u64().unwrap() > 0);
    assert!(lines[2]["usd"].as_f64().unwrap() > 0.0);
    let state = rig.state.lock().unwrap();
    assert!(state.seeds[0].done.is_none() && !state.queue[0].2 && state.proposed.is_empty());
    drop(state);
    assert_eq!(rig.gateway.requests().len(), 0);
}

#[test]
fn a_session_sent_again_after_a_cursor_reset_replaces_its_forest_record() {
    let rig = Rig::new(
        (1..=4)
            .map(|n| turn(&history_answer(n.min(3)), 100, 10))
            .collect(),
        history(),
    );
    let (stop, _, _) = rig.run(&rig.settings(), &[Stage::History], None);
    assert_eq!(stop, Stop::Done);
    // The owner resets the cursor: the first session is sent again.
    std::fs::remove_file(rig.dir.path("night").join("cursor.json")).unwrap();
    let (stop, totals, path) = rig.run(&rig.settings(), &[Stage::History], Some(1));
    assert_eq!(stop, Stop::SessionLimit);
    assert_eq!(totals.sessions, 1);
    let puts = forest_puts(&rig.archive);
    assert_eq!(puts.len(), 4);
    assert_eq!(puts[3].session, "s-1");
    assert_eq!(puts[3].links, puts[0].links);
    let line = report(path.as_ref().unwrap())
        .into_iter()
        .find(|l| l["type"] == "session")
        .unwrap();
    assert_eq!(line["forest"], 4);
    assert_eq!(line["forest_replaced"], json!([1]));
}
