//! The C7 facade through its public API. All people, places and texts
//! are fictional.

#![allow(missing_docs, clippy::unwrap_used, clippy::cast_precision_loss)]

use std::path::Path;

use ulei_garden::api::{ApiConfig, Garden, Hybrid, MAX_POINTERS};
use ulei_garden::miss::{self, ProducerKey, Queued};
use ulei_garden::model::{Domain, EntityId};
use ulei_garden::rerank::{NoReranker, Reranker};
use ulei_garden::search::{Hit, PointerGate, SearchError};
use ulei_garden::store::Store;
use ulei_garden::wire::c7;

fn garden(config: ApiConfig) -> Garden {
    garden_with(config, Box::new(NoReranker))
}

fn garden_with(config: ApiConfig, reranker: Box<dyn Reranker>) -> Garden {
    Garden::new(
        Store::open_in_memory().unwrap(),
        Hybrid::new(None, reranker),
        config,
        ProducerKey::from_bytes(&[0x5a; 32]).unwrap(),
    )
}

/// Every lexical candidate becomes a pointer: in a store of a few facts bm25 stays near zero,
/// far below the default gate, which is set on a larger corpus.
fn open_gate() -> ApiConfig {
    ApiConfig {
        pointers: PointerGate {
            min_cosine: 0.0,
            min_lexical: 0.0,
            ..PointerGate::default()
        },
        ..ApiConfig::default()
    }
}

fn propose(g: &mut Garden, entity: &str, fact: &str, now: i64) -> c7::ProposeResponse {
    let req = c7::ProposeRequest {
        fact: fact.into(),
        entity: Some(entity.into()),
        source_ref: "msg:7".into(),
        valid_from: Some(now as f64),
        kind: Some(c7::EntityKind::Person),
        domain: None,
        attribute: fact.contains("lives").then(|| "address".into()),
        valid_to: None,
        source_kind: Some(c7::SourceKind::Owner),
        observed_at: None,
        confidence: None,
        ttl_kind: None,
        secret_ref: None,
        private: None,
        pin: None,
    };
    decided(g, &req, now)
}

/// The answer to `req` on the night path at `now`: written into the seedbed, then processed at
/// once; a candidate refused at the write answers its refusal.
fn decided(g: &mut Garden, req: &c7::ProposeRequest, now: i64) -> c7::ProposeResponse {
    let written = g.garden_propose(req, now).unwrap();
    if written.status != c7::ProposalStatus::Seeded {
        return written;
    }
    let done = c7::SeedbedDoneRequest {
        items: vec![c7::SeedbedDoneItem {
            r#ref: written.proposal_id,
            candidates: None,
        }],
    };
    let mut items = g.garden_seedbed_done(&done, now).unwrap().items;
    items.remove(0).outcomes.remove(0)
}

fn search(q: &str, as_of: Option<f64>, private: bool) -> c7::SearchRequest {
    c7::SearchRequest {
        q: q.into(),
        k: None,
        as_of,
        private: Some(private),
        no_miss: None,
    }
}

/// Every column of the newest miss outbox row, as text.
fn newest_miss(g: &Garden) -> String {
    g.store()
        .conn()
        .query_row(
            "SELECT query_hash || '|' || payload || '|' || missed_at || '|' ||
                    coalesce(event_id, '') FROM miss_outbox ORDER BY seq DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

fn pointers(q: &str) -> c7::PointersRequest {
    c7::PointersRequest {
        q: q.into(),
        k: None,
        as_of: None,
        private: None,
    }
}

#[test]
fn search_returns_facts_with_ids_and_kinds() {
    let mut g = garden(ApiConfig::default());
    let r = propose(&mut g, "Ada Example", "Ada Example plays the cello.", 100);
    let a = g.garden_search(&search("cello", None, false), 200).unwrap();
    assert!(a.miss.is_none());
    let f = &a.response.facts[0];
    assert_eq!(f.fact, "Ada Example plays the cello.");
    assert_eq!(f.fact_id, r.fact_id);
    assert_eq!(f.kind, Some(c7::EntityKind::Person));
    assert_eq!(f.source_kind, Some(c7::SourceKind::Owner));
    assert!(f.score > 0.0);
}

#[test]
fn as_of_answers_what_the_garden_knew_then() {
    let mut g = garden(ApiConfig::default());
    propose(&mut g, "Ada Example", "Ada lives at 3 Old Road.", 1000);
    let r = propose(&mut g, "Ada Example", "Ada lives at 7 Sample Lane.", 2000);
    assert_eq!(r.status, c7::ProposalStatus::Accepted);
    let texts = |a: &c7::SearchResponse| a.facts.iter().map(|f| f.fact.clone()).collect::<Vec<_>>();
    let then = g
        .garden_search(&search("Ada lives", Some(1500.0), false), 3000)
        .unwrap();
    assert_eq!(texts(&then.response), ["Ada lives at 3 Old Road."]);
    let now = g
        .garden_search(&search("Ada lives", None, false), 3000)
        .unwrap();
    assert_eq!(texts(&now.response), ["Ada lives at 7 Sample Lane."]);
    let before = g
        .garden_search(&search("Ada lives", Some(10.0), false), 3000)
        .unwrap();
    assert_eq!(before.response.facts.len(), 0);
    assert_eq!(before.miss, Some(Queued::Added));
}

#[test]
fn private_facts_need_the_private_flag() {
    let mut g = garden(ApiConfig::default());
    let req = c7::ProposeRequest {
        fact: "Ada Example sees a cardiologist.".into(),
        entity: Some("Ada Example".into()),
        source_ref: "msg:9".into(),
        valid_from: None,
        kind: Some(c7::EntityKind::Person),
        domain: None,
        attribute: None,
        valid_to: None,
        source_kind: None,
        observed_at: None,
        confidence: None,
        ttl_kind: None,
        secret_ref: None,
        private: Some(true),
        pin: None,
    };
    decided(&mut g, &req, 100);
    let public = g
        .garden_search(&search("cardiologist", None, false), 200)
        .unwrap();
    assert_eq!(public.response.facts.len(), 0);
    let own = g
        .garden_search(&search("cardiologist", None, true), 200)
        .unwrap();
    assert_eq!(own.response.facts.len(), 1);
}

#[test]
fn pointers_never_carry_fact_text() {
    let mut g = garden(open_gate());
    for (i, f) in [
        "Ada Example prefers window seats on trains.",
        "Ada Example is allergic to peanuts.",
        "Ada Example takes the train on Fridays.",
        "Ada Example dislikes long train rides.",
        "Ada Example collects train tickets.",
    ]
    .iter()
    .enumerate()
    {
        propose(&mut g, "Ada Example", f, 100 + i64::try_from(i).unwrap());
    }
    let a = g
        .garden_pointers(&pointers("train trips for Ada"), 200)
        .unwrap();
    let p = &a.pointers;
    assert!(!p.is_empty() && p.len() <= MAX_POINTERS);
    let json = serde_json::to_string(&a).unwrap();
    for word in ["window", "peanuts", "Fridays", "tickets", "rides"] {
        assert!(!json.contains(word), "{word} leaked into {json}");
    }
    assert_eq!(p[0].hint, "there is a fact about Ada Example (person)");
    assert!(p[0].fact_id.starts_with('f'));
}

#[test]
fn nothing_above_the_pointer_threshold_queues_no_miss() {
    let mut g = garden(ApiConfig {
        pointers: PointerGate {
            min_cosine: 1.0,
            min_lexical: 1_000.0,
            ..PointerGate::default()
        },
        ..ApiConfig::default()
    });
    propose(&mut g, "Ada Example", "Ada Example plays the cello.", 100);
    let a = g.garden_pointers(&pointers("cello"), 200).unwrap();
    assert_eq!(a.pointers.len(), 0);
    let a = g
        .garden_pointers(&pointers("bassoon lessons"), 200)
        .unwrap();
    assert_eq!(a.pointers.len(), 0);
    assert_eq!(miss::unpublished(g.store()).unwrap(), 0);
}

#[test]
fn a_rejected_secret_cannot_be_found() {
    let mut g = garden(ApiConfig::default());
    let r = propose(&mut g, "Ada Example", "Ada's door code: 7Q4X9Z", 100);
    assert_eq!(r.status, c7::ProposalStatus::Rejected);
    assert_eq!(r.reason, Some(c7::ProposalReason::SecretValue));
    let a = g.garden_search(&search("7Q4X9Z", None, true), 200).unwrap();
    assert_eq!(a.response.facts.len(), 0);
    let a = g
        .garden_search(&search("door code", None, true), 200)
        .unwrap();
    assert_eq!(a.response.facts.len(), 0);
    let a = g
        .garden_search(&search("password: 7Q4X9Z", None, true), 200)
        .unwrap();
    assert_eq!(a.miss, Some(Queued::Added));
    let row = newest_miss(&g);
    assert!(
        !row.contains("7Q4X9Z"),
        "a secret-looking query is not kept: {row}"
    );
}

#[test]
fn spelled_pins_ids_and_phones_are_refused_and_never_echoed() {
    // Fictional values: SNILS below 001-001-998 and INN of region 00 are never issued (check
    // digits of 000-000-777 and 0000123456), 123-45-6789 is never assigned, area code 000 and
    // 555-01xx are unused.
    let cases = [
        (
            "Ada's intercom code is one two three four.",
            "one two three four",
            "intercom code one two three four",
        ),
        (
            "Ада: код домофона ноль семь ноль семь.",
            "ноль семь ноль семь",
            "код ноль семь ноль семь",
        ),
        ("Ada's bank PIN: 482.", "482", "PIN 482"),
        ("Код от квартиры: 4821", "4821", "Код от квартиры 4821"),
        ("Ada's mail password is 512.", "512", "password 512"),
        (
            "Ada's insurance number is 000-000-777 42.",
            "000-000-777 42",
            "000-000-777 42",
        ),
        (
            "Ada's tax number is 000012345627.",
            "000012345627",
            "000012345627",
        ),
        ("Ada's SSN is 123-45-6789.", "123-45-6789", "123-45-6789"),
        (
            "Паспорт Ады 00 00 123456.",
            "00 00 123456",
            "паспорт 00 00 123456",
        ),
        (
            "Ada's phone is +7 (000) 123-45-67.",
            "+7 (000) 123-45-67",
            "+7 (000) 123-45-67",
        ),
        (
            "Ada's office is 8 (000) 123-45-67.",
            "8 (000) 123-45-67",
            "8 (000) 123-45-67",
        ),
        (
            "Ada's US line is +1 (555) 010-0199.",
            "+1 (555) 010-0199",
            "+1 (555) 010-0199",
        ),
        (
            "Ada's Berlin line is +49 000 1234567.",
            "+49 000 1234567",
            "+49 000 1234567",
        ),
    ];
    let mut g = garden(ApiConfig::default());
    for (fact, value, query) in cases {
        let r = propose(&mut g, "Ada Example", fact, 100);
        assert_eq!(r.status, c7::ProposalStatus::Rejected, "{fact}");
        assert_eq!(r.reason, Some(c7::ProposalReason::SecretValue), "{fact}");
        let a = g.garden_search(&search(value, None, true), 200).unwrap();
        assert_eq!(a.response.facts.len(), 0, "{value}");
        let a = g.garden_search(&search(query, None, true), 200).unwrap();
        assert!(a.miss.is_some(), "{query}");
        let row = newest_miss(&g);
        assert!(!row.contains(value), "{query} kept: {row}");
    }
    assert!(
        g.store().find_entities("Ada Example").unwrap().is_empty(),
        "nothing written"
    );
}

#[test]
fn digest_and_resolve_over_the_wire_types() {
    let mut g = garden(ApiConfig::default());
    propose(&mut g, "Ada Example", "Ada lives at 3 Old Road.", 100);
    let weak = c7::ProposeRequest {
        fact: "Ada lives at 7 Sample Lane.".into(),
        entity: Some("Ada Example".into()),
        source_ref: "https://example.invalid/directory".into(),
        valid_from: Some(500.0),
        kind: None,
        domain: None,
        attribute: Some("address".into()),
        valid_to: None,
        source_kind: Some(c7::SourceKind::Internet),
        observed_at: None,
        confidence: None,
        ttl_kind: Some(c7::TtlKind::Place),
        secret_ref: None,
        private: None,
        pin: None,
    };
    let r = decided(&mut g, &weak, 500);
    assert_eq!(r.status, c7::ProposalStatus::Queued);
    let d = g.garden_digest(&c7::DigestRequest { limit: None }).unwrap();
    assert_eq!(d.items.len(), 1);
    assert_eq!(d.items[0].candidate, weak);
    let r = g
        .garden_resolve(
            &c7::ResolveRequest {
                proposal_id: r.proposal_id,
                action: c7::ResolveAction::Accept,
                replacement: None,
                entity: None,
            },
            600,
        )
        .unwrap();
    assert_eq!(r.status, c7::ProposalStatus::Accepted);
    assert_eq!(
        g.garden_digest(&c7::DigestRequest { limit: Some(5) })
            .unwrap()
            .items
            .len(),
        0
    );
}

fn fixture(dir: &str, name: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../contracts/fixtures/c7")
        .join(dir)
        .join(name);
    std::fs::read_to_string(p).unwrap()
}

#[test]
fn c7_fixtures_drive_the_api() {
    let mut g = garden(ApiConfig::default());
    g.store_mut()
        .upsert_domain(&Domain {
            slug: "doctors".into(),
            label: "Doctors".into(),
            wikidata: None,
            parent: None,
        })
        .unwrap();
    let full: c7::ProposeRequest =
        serde_json::from_str(&fixture("valid", "ProposeRequest.full.json")).unwrap();
    assert_eq!(
        decided(&mut g, &full, 1_700_000_200).status,
        c7::ProposalStatus::Accepted
    );
    let req: c7::PointersRequest =
        serde_json::from_str(&fixture("valid", "PointersRequest.basic.json")).unwrap();
    let a = g.garden_pointers(&req, 1_700_000_300).unwrap();
    assert!(a.pointers.len() <= MAX_POINTERS);
    let resolve: c7::ResolveRequest =
        serde_json::from_str(&fixture("valid", "ResolveRequest.replace.json")).unwrap();
    assert!(
        g.garden_resolve(&resolve, 1_700_000_400).is_err(),
        "no such open item"
    );
    for bad in ["ResolveRequest.bad_action.json", "Pointer.with_text.json"] {
        let text = fixture("invalid", bad);
        let parsed = if bad.starts_with("Resolve") {
            serde_json::from_str::<c7::ResolveRequest>(&text).is_ok()
        } else {
            serde_json::from_str::<c7::Pointer>(&text).is_ok()
        };
        assert!(!parsed, "{bad}");
    }
}

#[test]
fn hostile_queries_are_answered_not_failed() {
    let mut g = garden(ApiConfig::default());
    propose(&mut g, "Ada Example", "Ada Example plays the cello.", 100);
    for q in [
        "\"",
        "\"cello",
        "NEAR(",
        "NEAR(cello ada)",
        "*",
        "cello*",
        "-",
        "-cello",
        "AND",
        "OR NOT",
        "^",
        "ada:cello",
        "{}",
        "",
        "   ",
    ] {
        let s = c7::SearchRequest {
            k: Some(u64::MAX),
            ..search(q, None, false)
        };
        let a = g
            .garden_search(&s, 200)
            .unwrap_or_else(|e| panic!("{q:?}: {e}"));
        assert!(a.response.facts.len() <= 1, "{q:?}");
        let p = c7::PointersRequest {
            k: Some(u64::MAX),
            ..pointers(q)
        };
        let a = g
            .garden_pointers(&p, 200)
            .unwrap_or_else(|e| panic!("{q:?}: {e}"));
        assert!(a.pointers.len() <= 1, "{q:?}");
    }
    let s = c7::SearchRequest {
        k: Some(0),
        ..search("cello", None, false)
    };
    assert_eq!(g.garden_search(&s, 200).unwrap().response.facts.len(), 1);
}

#[test]
fn an_empty_search_is_a_garden_miss() {
    let mut g = garden(ApiConfig::default());
    propose(&mut g, "Ada Example", "Ada Example plays the cello.", 100);
    let a = g
        .garden_search(&search("harpsichord", None, false), 200)
        .unwrap();
    assert_eq!(a.response.facts.len(), 0);
    assert_eq!(a.miss, Some(Queued::Added));
    assert_eq!(miss::unpublished(g.store()).unwrap(), 1);
    let row = newest_miss(&g);
    assert!(
        row.contains("The garden has nothing for: harpsichord|200|"),
        "{row}"
    );
}

#[test]
fn pointers_to_private_facts_need_the_private_flag() {
    let mut g = garden(open_gate());
    let req = c7::ProposeRequest {
        fact: "Ada Example sees a cardiologist.".into(),
        entity: Some("Ada Example".into()),
        source_ref: "msg:9".into(),
        valid_from: None,
        kind: Some(c7::EntityKind::Person),
        domain: None,
        attribute: None,
        valid_to: None,
        source_kind: None,
        observed_at: None,
        confidence: None,
        ttl_kind: None,
        secret_ref: None,
        private: Some(true),
        pin: None,
    };
    decided(&mut g, &req, 100);
    let public = g.garden_pointers(&pointers("cardiologist"), 200).unwrap();
    assert_eq!(public.pointers.len(), 0);
    let own = c7::PointersRequest {
        private: Some(true),
        ..pointers("cardiologist")
    };
    let own = g.garden_pointers(&own, 200).unwrap();
    assert_eq!(own.pointers.len(), 1);
    assert_eq!(own.pointers[0].entity, "Ada Example");
}

#[test]
fn trivial_prompts_retrieve_nothing_and_miss_nothing() {
    let mut g = garden(open_gate());
    propose(
        &mut g,
        "Ada Example",
        "Ada Example says thanks with flowers.",
        100,
    );
    for q in ["ok!", "Thanks!", "thank you.", "спасибо", "  ", "", "/help"] {
        let a = g.garden_pointers(&pointers(q), 200).unwrap();
        assert_eq!(a.pointers.len(), 0, "{q:?}");
        let a = g.garden_search(&search(q, None, false), 200).unwrap();
        assert_eq!((a.response.facts.len(), a.miss), (0, None), "{q:?}");
    }
    assert_eq!(miss::unpublished(g.store()).unwrap(), 0);
    let a = g.garden_pointers(&pointers("flowers"), 200).unwrap();
    assert_eq!(a.pointers.len(), 1);
}

/// Reverses the fused order.
struct Reverse;

impl Reranker for Reverse {
    fn rerank(&self, _: &Store, _: &str, hits: &mut [Hit]) -> Result<(), SearchError> {
        hits.reverse();
        Ok(())
    }
}

/// Fact ids and scores of `search` for "cello" with `k`, over three facts of distinct ranks.
fn cello_search(reranker: Box<dyn Reranker>, k: Option<u64>) -> Vec<(String, f64)> {
    let mut g = garden_with(ApiConfig::default(), reranker);
    for (i, (entity, fact)) in [
        (
            "Ada Example",
            "Ada Example plays the cello, a cello by a cello maker.",
        ),
        ("Ben Sample", "Ben Sample tunes a cello and a viola."),
        (
            "Cy Example",
            "Cy Example has a cello case among many old instrument cases in the attic.",
        ),
    ]
    .iter()
    .enumerate()
    {
        propose(&mut g, entity, fact, 100 + i64::try_from(i).unwrap());
    }
    let req = c7::SearchRequest {
        k,
        ..search("cello", None, false)
    };
    g.garden_search(&req, 200)
        .unwrap()
        .response
        .facts
        .into_iter()
        .map(|f| (f.fact_id.unwrap(), f.score))
        .collect()
}

#[test]
fn the_reranker_orders_search_before_the_cut_and_none_keeps_the_fused_order() {
    let fused = cello_search(Box::new(NoReranker), None);
    assert_eq!(fused.len(), 3);
    assert!(fused.windows(2).all(|w| w[0].1 > w[1].1), "{fused:?}");

    let mut reversed = cello_search(Box::new(Reverse), None);
    reversed.reverse();
    assert_eq!(reversed, fused);

    let first = cello_search(Box::new(Reverse), Some(1));
    assert_eq!(first, [fused[2].clone()], "the cut takes the reranked head");
}

/// The proof count `search` reports for the one fact it finds for `q` at `now`.
fn proofs(g: &mut Garden, q: &str, now: i64) -> (String, Option<u64>) {
    let r = g.garden_search(&search(q, None, false), now).unwrap();
    assert_eq!(r.response.facts.len(), 1, "{q}");
    let f = &r.response.facts[0];
    (f.fact_id.clone().unwrap(), f.proof_count)
}

fn duplicate(g: &mut Garden, entity: &str, fact: &str, source: &str, now: i64) {
    let req = c7::ProposeRequest {
        source_ref: source.into(),
        ..fixed_request(entity, fact)
    };
    let r = decided(g, &req, now);
    assert_eq!(r.status, c7::ProposalStatus::Duplicate, "{source}");
}

fn fixed_request(entity: &str, fact: &str) -> c7::ProposeRequest {
    c7::ProposeRequest {
        fact: fact.into(),
        entity: Some(entity.into()),
        source_ref: "msg:1".into(),
        valid_from: Some(100.0),
        kind: Some(c7::EntityKind::Person),
        domain: None,
        attribute: None,
        valid_to: None,
        source_kind: Some(c7::SourceKind::Owner),
        observed_at: None,
        confidence: None,
        ttl_kind: None,
        secret_ref: None,
        private: None,
        pin: None,
    }
}

#[test]
fn two_duplicates_raise_the_proof_count_to_three() {
    let mut g = garden(ApiConfig::default());
    let first = decided(
        &mut g,
        &fixed_request("Ada Example", "Ada plays the oboe."),
        100,
    );
    let fact = first.fact_id.unwrap();
    assert_eq!(proofs(&mut g, "oboe", 150), (fact.clone(), Some(1)));
    duplicate(&mut g, "Ada Example", "Ada plays the oboe.", "msg:2", 200);
    duplicate(&mut g, "Ada Example", "ada plays the OBOE", "msg:3", 300);
    assert_eq!(proofs(&mut g, "oboe", 350), (fact, Some(3)));
}

#[test]
fn the_proof_count_follows_a_fact_to_its_copy_on_the_survivor_of_a_merge() {
    let mut g = garden(ApiConfig::default());
    decided(
        &mut g,
        &fixed_request("Ada Example", "Ada likes rain."),
        100,
    );
    let moved = decided(
        &mut g,
        &fixed_request("A. Sample", "She plays the oboe."),
        100,
    )
    .fact_id
    .unwrap();
    duplicate(&mut g, "A. Sample", "She plays the oboe.", "msg:2", 200);
    duplicate(&mut g, "A. Sample", "She plays the oboe.", "msg:3", 200);
    g.store_mut()
        .merge_entities(
            &EntityId("a-sample".into()),
            &EntityId("ada-example".into()),
            300,
        )
        .unwrap();
    let (copy, count) = proofs(&mut g, "oboe", 350);
    assert_ne!(copy, moved);
    assert_eq!(count, Some(3));
    duplicate(&mut g, "Ada Example", "She plays the oboe.", "msg:4", 400);
    assert_eq!(proofs(&mut g, "oboe", 450), (copy, Some(4)));
}

/// Rows of the tables a write may touch: facts, entities, proposals, misses and records.
fn rows(g: &Garden) -> [i64; 5] {
    [
        "facts",
        "entities",
        "edge_proposal",
        "miss_outbox",
        "seedbed",
    ]
    .map(|table| {
        g.store()
            .conn()
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    })
}

fn open_items(g: &mut Garden) -> usize {
    g.garden_digest(&c7::DigestRequest { limit: None })
        .unwrap()
        .items
        .len()
}

fn address(entity: &str, fact: &str, source: &str) -> c7::ProposeRequest {
    c7::ProposeRequest {
        attribute: Some("address".into()),
        source_ref: source.into(),
        ..fixed_request(entity, fact)
    }
}

/// The stored record of a `seeded` answer.
fn record(g: &Garden, answer: &c7::ProposeResponse) -> ulei_garden::store::SeedRecord {
    assert_eq!(answer.status, c7::ProposalStatus::Seeded, "{answer:?}");
    let seq = answer.proposal_id.strip_prefix("seed:").unwrap();
    g.store().seed(seq.parse().unwrap()).unwrap().unwrap()
}

#[test]
fn a_remember_this_is_seeded_under_its_ref_and_writes_no_fact_no_proposal_and_no_digest_item() {
    let mut g = garden(ApiConfig::default());
    let said = fixed_request("Ada Example", "Ada plays the oboe.");
    let r = g.garden_propose(&said, 200).unwrap();
    assert_eq!(
        r,
        c7::ProposeResponse {
            proposal_id: "seed:1".into(),
            status: c7::ProposalStatus::Seeded,
            fact_id: None,
            reason: None,
            conflicts_with: None,
        }
    );
    let stored = record(&g, &r);
    assert_eq!((stored.candidate, stored.written_at), (said, 200));
    assert_eq!(stored.processed, None);
    assert_eq!(rows(&g), [0, 0, 0, 0, 1]);
    assert_eq!(open_items(&mut g), 0);
}

#[test]
fn another_value_of_a_current_attribute_is_disputed_and_an_equal_one_or_an_unresolved_name_is_not()
{
    let mut g = garden(ApiConfig::default());
    let current = decided(
        &mut g,
        &address("Ada Example", "Ada lives at 3 Old Road.", "msg:1"),
        100,
    );
    let current = current.fact_id.unwrap();
    for name in ["Mira Velde", "Мира Валда"] {
        let home = address(name, &format!("{name} lives by the river."), "msg:1");
        assert_eq!(
            decided(&mut g, &home, 100).status,
            c7::ProposalStatus::Accepted
        );
    }
    let fair = c7::ProposeRequest {
        kind: Some(c7::EntityKind::Event),
        ..address("Spring Fair", "The Spring Fair is on Mill Square.", "msg:1")
    };
    assert_eq!(
        decided(&mut g, &fair, 100).status,
        c7::ProposalStatus::Accepted
    );
    let before = rows(&g);

    let moved = g
        .garden_propose(
            &address("Ada Example", "Ada lives at 7 Sample Lane.", "msg:2"),
            200,
        )
        .unwrap();
    assert_eq!(moved.conflicts_with.as_deref(), Some(current.as_str()));
    let stored = record(&g, &moved);
    assert_eq!(stored.conflicts_with.map(|f| f.0), Some(current));
    assert_eq!(stored.entity.map(|e| e.0).as_deref(), Some("ada-example"));

    let same = address("Ada Example", "ada lives at 3 OLD Road", "msg:3");
    let ambiguous = address("Mira Valde", "Mira lives on a boat.", "msg:2");
    let other_fair = c7::ProposeRequest {
        kind: Some(c7::EntityKind::Event),
        ..address("Spring Fair", "The Spring Fair is in the park.", "msg:2")
    };
    for (candidate, entity) in [
        (&same, Some("ada-example")),
        (&ambiguous, None),
        (&other_fair, None),
    ] {
        let r = g.garden_propose(candidate, 200).unwrap();
        assert_eq!(r.conflicts_with, None, "{}", candidate.fact);
        let stored = record(&g, &r);
        assert_eq!(stored.conflicts_with, None, "{}", candidate.fact);
        assert_eq!(
            stored.entity.map(|e| e.0).as_deref(),
            entity,
            "{}",
            candidate.fact
        );
    }
    let [facts, entities, proposals, misses, records] = before;
    assert_eq!(rows(&g), [facts, entities, proposals, misses, records + 4]);
    assert_eq!(open_items(&mut g), 0);
}

#[test]
fn a_secret_value_and_a_candidate_past_its_expiry_are_refused_with_only_a_proposal_row() {
    let mut g = garden(ApiConfig::default());
    let secret = fixed_request("Ada Example", "Ada's door code: 7Q4X9Z");
    let stale = c7::ProposeRequest {
        source_kind: Some(c7::SourceKind::Internet),
        ttl_kind: Some(c7::TtlKind::Weather),
        observed_at: Some(1_000.0),
        ..fixed_request("Fogtown", "Fogtown expects drizzle.")
    };
    let refused = [
        (&secret, c7::ProposalReason::SecretValue),
        (&stale, c7::ProposalReason::Invalid),
    ];
    for (n, (candidate, reason)) in (1_i64..).zip(refused) {
        let r = g.garden_propose(candidate, 1_000 + 3 * 3_600).unwrap();
        assert_eq!(r.status, c7::ProposalStatus::Rejected, "{}", candidate.fact);
        assert_eq!(r.reason, Some(reason), "{}", candidate.fact);
        assert_eq!(rows(&g), [0, 0, n, 0, 0], "{}", candidate.fact);
    }
    let fresh = g.garden_propose(&stale, 1_000 + 3 * 3_600 - 1).unwrap();
    assert_eq!(fresh.status, c7::ProposalStatus::Seeded);
}

#[test]
fn a_repeated_candidate_answers_its_unexpired_record_and_another_source_is_another_record() {
    let mut g = garden(ApiConfig::default());
    let current = decided(
        &mut g,
        &address("Ada Example", "Ada lives at 3 Old Road.", "msg:1"),
        100,
    );
    let moved = address("Ada Example", "Ada lives at 7 Sample Lane.", "msg:2");
    let first = g.garden_propose(&moved, 200).unwrap();
    assert_eq!(first.conflicts_with, current.fact_id);
    let before = rows(&g);

    let restated = c7::ProposeRequest {
        fact: "  ada lives at 7 SAMPLE lane ".into(),
        ..moved.clone()
    };
    assert_eq!(g.garden_propose(&restated, 300).unwrap(), first);
    let done = c7::SeedbedDoneRequest {
        items: vec![c7::SeedbedDoneItem {
            r#ref: first.proposal_id.clone(),
            candidates: None,
        }],
    };
    g.garden_seedbed_done(&done, 400).unwrap();
    let after_night = rows(&g);
    assert_eq!(g.garden_propose(&moved, 500).unwrap(), first, "processed");
    assert_eq!(rows(&g), after_night);
    assert_eq!(after_night[4], before[4]);

    let elsewhere = c7::ProposeRequest {
        source_ref: "msg:3".into(),
        ..moved.clone()
    };
    let other = g.garden_propose(&elsewhere, 500).unwrap();
    assert_ne!(other.proposal_id, first.proposal_id);
    assert_eq!(rows(&g)[4], before[4] + 1);

    let weather = c7::ProposeRequest {
        source_kind: Some(c7::SourceKind::Internet),
        ttl_kind: Some(c7::TtlKind::Weather),
        ..fixed_request("Fogtown", "Fogtown expects drizzle.")
    };
    let morning = g.garden_propose(&weather, 1_000).unwrap();
    assert_eq!(
        g.garden_propose(&weather, 1_000 + 3 * 3_600 - 1).unwrap(),
        morning
    );
    let evening = g.garden_propose(&weather, 1_000 + 3 * 3_600).unwrap();
    assert_ne!(
        evening.proposal_id, morning.proposal_id,
        "the first one expired"
    );
}

#[test]
fn a_candidate_equal_to_a_forgotten_record_writes_a_new_record() {
    let mut g = garden(ApiConfig::default());
    let moved = address("Ada Example", "Ada lives at 7 Sample Lane.", "msg:2");
    let first = g.garden_propose(&moved, 200).unwrap();
    let forget = c7::ForgetRequest {
        refs: vec![first.proposal_id.clone()],
        reason: Some("Not true.".into()),
        provenance: "s1-t1".into(),
        undo: None,
    };
    g.garden_forget(&forget, 300).unwrap();
    let before = rows(&g);

    let again = g.garden_propose(&moved, 400).unwrap();
    assert_eq!(again.status, c7::ProposalStatus::Seeded);
    assert_ne!(again.proposal_id, first.proposal_id);
    assert_eq!(rows(&g)[4], before[4] + 1);
    assert_eq!(g.garden_propose(&moved, 500).unwrap(), again);
}

/// Fact ids `search` answers for `q` at `now`.
fn found_ids(g: &mut Garden, q: &str, private: bool, now: i64) -> Vec<String> {
    g.garden_search(&search(q, None, private), now)
        .unwrap()
        .response
        .facts
        .into_iter()
        .map(|f| f.fact_id.unwrap())
        .collect()
}

#[test]
fn a_record_is_found_as_an_unprocessed_fact_until_its_expiry_and_then_the_question_is_a_miss() {
    let mut g = garden(ApiConfig::default());
    let weather = c7::ProposeRequest {
        kind: Some(c7::EntityKind::Place),
        source_kind: Some(c7::SourceKind::Internet),
        ttl_kind: Some(c7::TtlKind::Weather),
        observed_at: Some(1_000.0),
        ..fixed_request("Fogtown", "Fogtown expects drizzle.")
    };
    let written = g.garden_propose(&weather, 1_000).unwrap();
    let q = search("drizzle in Fogtown", None, false);

    let day = g.garden_search(&q, 2_000).unwrap();
    assert_eq!(
        day.miss, None,
        "a search that finds only a record is no miss"
    );
    let [found] = day.response.facts.as_slice() else {
        panic!("{day:?}");
    };
    assert!(found.score > 0.0);
    assert_eq!(
        found,
        &c7::Fact {
            fact: "Fogtown expects drizzle.".into(),
            entity: "Fogtown".into(),
            valid_from: 100.0,
            valid_to: None,
            source_ref: "msg:1".into(),
            score: found.score,
            fact_id: Some(written.proposal_id),
            kind: Some(c7::EntityKind::Place),
            domain: None,
            source_kind: Some(c7::SourceKind::Internet),
            expires_at: Some(f64::from(1_000 + 3 * 3_600)),
            pinned: None,
            proof_count: None,
            recheck_before_acting: None,
            unprocessed: Some(true),
            disputed: None,
            conflicts_with: None,
        }
    );

    let expired = g.garden_search(&q, 1_000 + 3 * 3_600).unwrap();
    assert_eq!(expired.response.facts, []);
    assert!(expired.miss.is_some(), "the same question is a miss again");
}

#[test]
fn a_record_whose_valid_to_has_passed_is_not_returned_for_now() {
    let mut g = garden(ApiConfig::default());
    let away = c7::ProposeRequest {
        valid_to: Some(500.0),
        ..fixed_request("Ada Example", "Ada is away in Lisbon.")
    };
    let written = g.garden_propose(&away, 200).unwrap();
    assert_eq!(
        found_ids(&mut g, "Lisbon", false, 300),
        std::slice::from_ref(&written.proposal_id)
    );
    assert_eq!(
        found_ids(&mut g, "Lisbon", false, 600),
        Vec::<String>::new()
    );
    let then = g
        .garden_search(&search("Lisbon", Some(300.0), false), 600)
        .unwrap();
    assert_eq!(then.response.facts.len(), 1, "as of a time it held");
}

#[test]
fn a_private_record_is_found_only_with_private() {
    let mut g = garden(open_gate());
    let own = c7::ProposeRequest {
        private: Some(true),
        ..fixed_request("Ada Example", "Ada sees a therapist on Tuesdays.")
    };
    let written = g.garden_propose(&own, 200).unwrap();
    assert_eq!(
        found_ids(&mut g, "therapist", false, 300),
        Vec::<String>::new()
    );
    assert_eq!(
        found_ids(&mut g, "therapist", true, 300),
        std::slice::from_ref(&written.proposal_id)
    );
    let pointed = |g: &Garden, private| {
        let req = c7::PointersRequest {
            private,
            ..pointers("therapist")
        };
        g.garden_pointers(&req, 300)
            .unwrap()
            .pointers
            .into_iter()
            .map(|p| p.fact_id)
            .collect::<Vec<_>>()
    };
    assert_eq!(pointed(&g, None), Vec::<String>::new());
    assert_eq!(pointed(&g, Some(true)), [written.proposal_id]);
}

#[test]
fn record_pointers_name_the_entity_and_kind_one_per_entity_and_none_without_an_entity() {
    let mut g = garden(open_gate());
    let current = decided(
        &mut g,
        &address("Ada Example", "Ada lives at 3 Old Road.", "msg:1"),
        100,
    )
    .fact_id
    .unwrap();
    let hive = |entity: &str, fact: &str, source: &str| c7::ProposeRequest {
        source_ref: source.into(),
        ..fixed_request(entity, fact)
    };
    let bram = g
        .garden_propose(
            &hive("Bram Sample", "Bram Sample keeps bees.", "msg:2"),
            200,
        )
        .unwrap();
    let honey = hive("bram  sample", "Bram sells honey from his bees.", "msg:3");
    let honey = g.garden_propose(&honey, 200).unwrap();
    let cleo = c7::ProposeRequest {
        kind: None,
        ..hive("Cleo Sample", "Cleo Sample watches the bees.", "msg:4")
    };
    let cleo = g.garden_propose(&cleo, 200).unwrap();
    let lifted = c7::ProposeRequest {
        entity: None,
        kind: None,
        source_kind: Some(c7::SourceKind::Raw),
        source_ref: "archive:41".into(),
        ..fixed_request("", "The bees swarmed near the clinic.")
    };
    let lifted = g.garden_propose(&lifted, 200).unwrap();
    let moved = address("Ada Example", "Ada lives at the bees yard.", "msg:5");
    let moved = g.garden_propose(&moved, 200).unwrap();
    assert_eq!(moved.conflicts_with.as_deref(), Some(current.as_str()));

    let mut answer = g.garden_pointers(&pointers("bees"), 300).unwrap().pointers;
    answer.sort_by(|a, b| a.entity.cmp(&b.entity));
    let shape = |p: &c7::Pointer| {
        (
            p.entity.clone(),
            p.kind,
            p.hint.clone(),
            p.unprocessed,
            p.disputed,
        )
    };
    let person = c7::EntityKind::Person;
    assert_eq!(
        answer.iter().map(shape).collect::<Vec<_>>(),
        [
            (
                "Ada Example".to_owned(),
                person,
                "there is an unprocessed record about Ada Example (person, disputed)".to_owned(),
                Some(true),
                Some(true),
            ),
            (
                "Bram Sample".to_owned(),
                person,
                "there is an unprocessed record about Bram Sample (person)".to_owned(),
                Some(true),
                None,
            ),
            (
                "Cleo Sample".to_owned(),
                c7::EntityKind::Item,
                "there is an unprocessed record about Cleo Sample (item)".to_owned(),
                Some(true),
                None,
            ),
        ]
    );
    assert_eq!(answer[0].fact_id, moved.proposal_id);
    assert!([&bram.proposal_id, &honey.proposal_id].contains(&&answer[1].fact_id));
    assert_eq!(answer[2].fact_id, cleo.proposal_id);

    let facts = g
        .garden_search(&search("bees", None, false), 300)
        .unwrap()
        .response
        .facts;
    let of = |r: &c7::ProposeResponse| {
        facts
            .iter()
            .find(|f| f.fact_id.as_ref() == Some(&r.proposal_id))
            .unwrap()
    };
    assert_eq!(
        of(&lifted).entity,
        "",
        "a record without an entity is still found"
    );
    assert_eq!(of(&moved).entity, "ada-example", "the resolved entity's id");
    assert_eq!(of(&moved).disputed, Some(true));
    assert_eq!(of(&moved).conflicts_with.as_deref(), Some(current.as_str()));
}

#[test]
fn a_waiting_record_is_found_by_its_entitys_new_alias_and_after_a_merge_by_the_survivors() {
    let mut g = garden(ApiConfig::default());
    for (entity, fact) in [
        ("Ada Example", "Ada likes rain."),
        ("A. Sample", "She likes snow."),
    ] {
        decided(&mut g, &fixed_request(entity, fact), 100);
    }
    let hobby = |entity: &str, fact: &str| c7::ProposeRequest {
        attribute: Some("hobby".into()),
        source_ref: "msg:2".into(),
        ..fixed_request(entity, fact)
    };
    let bees = g
        .garden_propose(&hobby("Ada Example", "She keeps bees."), 200)
        .unwrap();
    let oboe = g
        .garden_propose(&hobby("A. Sample", "She plays the oboe."), 200)
        .unwrap();
    assert_eq!(record(&g, &oboe).entity, Some(EntityId("a-sample".into())));
    assert_eq!(found_ids(&mut g, "Adka", false, 300), Vec::<String>::new());

    let ada = EntityId("ada-example".into());
    let mut renamed = g.store().entity(&ada).unwrap().unwrap();
    renamed.aliases.push("Adka".into());
    g.store_mut().upsert_entity(&renamed).unwrap();
    assert!(found_ids(&mut g, "Adka", false, 300).contains(&bees.proposal_id));

    g.store_mut()
        .merge_entities(&EntityId("a-sample".into()), &ada, 300)
        .unwrap();
    assert!(found_ids(&mut g, "Adka", false, 400).contains(&oboe.proposal_id));
}

#[test]
fn a_record_pointer_trims_the_written_name_and_after_a_merge_names_the_survivor() {
    let mut g = garden(open_gate());
    for (entity, fact) in [
        ("Ada Example", "Ada likes rain."),
        ("A. Sample", "She likes snow."),
    ] {
        decided(&mut g, &fixed_request(entity, fact), 100);
    }
    let oboe = c7::ProposeRequest {
        attribute: Some("hobby".into()),
        source_ref: "msg:2".into(),
        ..fixed_request("A. Sample", "She plays the oboe.")
    };
    let oboe = g.garden_propose(&oboe, 200).unwrap();
    let bees = fixed_request("  Bram Sample ", "Bram keeps bees.");
    let bees = g.garden_propose(&bees, 200).unwrap();
    let named = |g: &Garden, q: &str, now: i64| {
        g.garden_pointers(&pointers(q), now)
            .unwrap()
            .pointers
            .into_iter()
            .map(|p| (p.fact_id, p.entity, p.hint))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        named(&g, "bees", 300),
        [(
            bees.proposal_id,
            "Bram Sample".to_owned(),
            "there is an unprocessed record about Bram Sample (person)".to_owned(),
        )]
    );

    let ada = EntityId("ada-example".into());
    g.store_mut()
        .merge_entities(&EntityId("a-sample".into()), &ada, 300)
        .unwrap();
    assert_eq!(
        named(&g, "oboe", 400),
        [(
            oboe.proposal_id.clone(),
            "Ada Example".to_owned(),
            "there is an unprocessed record about Ada Example (person)".to_owned(),
        )]
    );
    let facts = g
        .garden_search(&search("oboe", None, false), 400)
        .unwrap()
        .response
        .facts;
    assert_eq!(facts[0].fact_id.as_ref(), Some(&oboe.proposal_id));
    assert_eq!(facts[0].entity, ada.0, "the survivor's id");
}
