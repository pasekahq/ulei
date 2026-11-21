//! The C7 facade through its public API. All people, places and texts
//! are fictional.

#![allow(missing_docs, clippy::unwrap_used, clippy::cast_precision_loss)]

use std::path::Path;

use ulei_garden::api::{ApiConfig, Garden, Lexical, MAX_POINTERS};
use ulei_garden::model::Domain;
use ulei_garden::store::Store;
use ulei_garden::wire::{c1, c7};

fn garden(config: ApiConfig) -> Garden {
    Garden::new(Store::open_in_memory().unwrap(), Lexical, config).unwrap()
}

fn propose(g: &mut Garden, entity: &str, fact: &str, now: i64) -> c7::ProposeResponse {
    let req = c7::ProposeRequest {
        fact: fact.into(),
        entity: entity.into(),
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
    g.garden_propose(&req, now).unwrap()
}

fn search(q: &str, as_of: Option<f64>, private: bool) -> c7::SearchRequest {
    c7::SearchRequest {
        q: q.into(),
        k: None,
        as_of,
        private: Some(private),
    }
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
    assert!(before.miss.is_some());
}

#[test]
fn private_facts_need_the_private_flag() {
    let mut g = garden(ApiConfig::default());
    let req = c7::ProposeRequest {
        fact: "Ada Example sees a cardiologist.".into(),
        entity: "Ada Example".into(),
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
    g.garden_propose(&req, 100).unwrap();
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
    let mut g = garden(ApiConfig::default());
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
    assert!(a.miss.is_none());
    let p = &a.response.pointers;
    assert!(!p.is_empty() && p.len() <= MAX_POINTERS);
    let json = serde_json::to_string(&a.response).unwrap();
    for word in ["window", "peanuts", "Fridays", "tickets", "rides"] {
        assert!(!json.contains(word), "{word} leaked into {json}");
    }
    assert_eq!(p[0].hint, "there is a fact about Ada Example (person)");
    assert!(p[0].fact_id.starts_with('f'));
}

#[test]
fn nothing_above_the_threshold_is_a_garden_miss() {
    let mut g = garden(ApiConfig {
        pointer_min_score: 1_000.0,
        ..ApiConfig::default()
    });
    propose(&mut g, "Ada Example", "Ada Example plays the cello.", 100);
    let a = g.garden_pointers(&pointers("cello"), 200).unwrap();
    assert_eq!(a.response.pointers.len(), 0);
    let miss = a.miss.expect("a miss");
    let event = miss.into_event("0".repeat(64));
    assert_eq!(event.kind, c1::Kind::GardenMiss);
    assert_eq!(event.source_ts, 200.0);
    assert!(event.payload.contains("cello"));
    assert!(event.id.len() <= 96);
    assert!(
        event
            .id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b":_-".contains(&b))
    );
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
    let miss = g
        .garden_pointers(&pointers("password: 7Q4X9Z"), 200)
        .unwrap()
        .miss
        .unwrap();
    assert!(
        !miss.payload.contains("7Q4X9Z"),
        "a secret-looking query is not echoed"
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
            "один два три четыре",
            "intercom code one two three four",
        ),
        (
            "Ада: код домофона ноль семь ноль семь.",
            "ноль семь ноль семь",
            "код ноль семь ноль семь",
        ),
        ("Ada's bank PIN: 482.", "482", "PIN 482"),
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
        let miss = g
            .garden_pointers(&pointers(query), 200)
            .unwrap()
            .miss
            .unwrap();
        assert!(
            !miss.payload.contains(value),
            "{query} echoed: {}",
            miss.payload
        );
        let miss = g
            .garden_search(&search(query, None, true), 200)
            .unwrap()
            .miss
            .unwrap();
        assert!(
            !miss.payload.contains(value),
            "{query} echoed: {}",
            miss.payload
        );
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
        entity: "Ada Example".into(),
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
    let r = g.garden_propose(&weak, 500).unwrap();
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
        g.garden_propose(&full, 1_700_000_200).unwrap().status,
        c7::ProposalStatus::Accepted
    );
    let req: c7::PointersRequest =
        serde_json::from_str(&fixture("valid", "PointersRequest.basic.json")).unwrap();
    let a = g.garden_pointers(&req, 1_700_000_300).unwrap();
    assert!(a.response.pointers.len() <= MAX_POINTERS);
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
        assert!(a.response.pointers.len() <= 1, "{q:?}");
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
    let miss = a.miss.expect("a miss");
    assert!(miss.payload.contains("harpsichord"));
    assert_eq!(miss.source_ts, 200.0);
}

#[test]
fn pointers_to_private_facts_need_the_private_flag() {
    let mut g = garden(ApiConfig::default());
    let req = c7::ProposeRequest {
        fact: "Ada Example sees a cardiologist.".into(),
        entity: "Ada Example".into(),
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
    g.garden_propose(&req, 100).unwrap();
    let public = g.garden_pointers(&pointers("cardiologist"), 200).unwrap();
    assert_eq!(public.response.pointers.len(), 0);
    assert!(public.miss.is_some());
    let own = c7::PointersRequest {
        private: Some(true),
        ..pointers("cardiologist")
    };
    let own = g.garden_pointers(&own, 200).unwrap();
    assert_eq!(own.response.pointers.len(), 1);
    assert_eq!(own.response.pointers[0].entity, "Ada Example");
}

#[test]
fn trivial_prompts_retrieve_nothing_and_miss_nothing() {
    let mut g = garden(ApiConfig::default());
    propose(
        &mut g,
        "Ada Example",
        "Ada Example says thanks with flowers.",
        100,
    );
    for q in ["ok!", "Thanks!", "thank you.", "спасибо", "  ", "", "/help"] {
        let a = g.garden_pointers(&pointers(q), 200).unwrap();
        assert_eq!((a.response.pointers.len(), a.miss), (0, None), "{q:?}");
        let a = g.garden_search(&search(q, None, false), 200).unwrap();
        assert_eq!((a.response.facts.len(), a.miss), (0, None), "{q:?}");
    }
    let a = g.garden_pointers(&pointers("flowers"), 200).unwrap();
    assert_eq!(a.response.pointers.len(), 1);
}
