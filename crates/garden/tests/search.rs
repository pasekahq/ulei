//! Hybrid search and passive pointers over a hand-built store: both time axes on the dense and
//! the lexical side, private and secret facts, parameter checks. All people, places and texts
//! are fictional.

#![allow(missing_docs, clippy::unwrap_used)]

use ulei_garden::embed::HashEmbedder;
use ulei_garden::model::{
    Attrs, Entity, EntityId, EntityKind, Fact, FactId, Provenance, Sensitivity, SourceKind, TtlKind,
};
use ulei_garden::rerank::NoReranker;
use ulei_garden::search::{
    PointerGate, SearchError, SearchParams, Searcher, index_missing, pointers,
};
use ulei_garden::store::{Indexed, Store};

const DAY: i64 = 86_400;
const NOW: i64 = 1_800_000_000;

fn hit(id: &str) -> Indexed {
    Indexed::Fact(FactId(id.into()))
}

fn fact(id: &str, text: &str, valid: (i64, Option<i64>), recorded_at: i64) -> Fact {
    Fact {
        id: FactId(id.into()),
        entity: EntityId("p-mira".into()),
        text: text.into(),
        domain: None,
        attribute: None,
        ttl: TtlKind::Personal,
        valid_from: valid.0,
        valid_to: valid.1,
        recorded_at,
        superseded_at: None,
        superseded_by: None,
        provenance: Provenance {
            kind: SourceKind::Owner,
            source_ref: format!("msg:{id}"),
            observed_at: recorded_at,
            quote: None,
            proof_count: 1,
        },
        expires_at: None,
        sensitivity: Sensitivity::Public,
        confidence: 1.0,
    }
}

/// Mira moved from Alder Street to Birch Road ten days ago; the garden learnt the new address
/// then. She also has a private and a secret fact.
fn garden(e: &HashEmbedder) -> Store {
    let mut s = Store::open_in_memory().unwrap();
    s.upsert_entity(&Entity {
        id: EntityId("p-mira".into()),
        kind: EntityKind::Person,
        name: "Mira Velde".into(),
        aliases: vec![],
        domain: None,
        attrs: Attrs::None,
    })
    .unwrap();
    let moved = NOW - 10 * DAY;
    let facts = [
        fact(
            "f-alder",
            "Lives at 4 Alder Street",
            (NOW - 400 * DAY, Some(moved)),
            NOW - 400 * DAY,
        ),
        fact("f-birch", "Lives at 9 Birch Road", (moved, None), moved),
        Fact {
            sensitivity: Sensitivity::Private,
            ..fact(
                "f-therapist",
                "Sees the therapist Orrin Hale on Thursdays",
                (NOW - 100 * DAY, None),
                NOW - 100 * DAY,
            )
        },
        Fact {
            sensitivity: Sensitivity::Secret {
                vault_ref: "vault:entry-7".into(),
            },
            ..fact(
                "f-pin",
                "Bank card PIN exists and is kept in the vault",
                (NOW - 100 * DAY, None),
                NOW - 100 * DAY,
            )
        },
    ];
    for f in &facts {
        s.insert_fact(f).unwrap();
    }
    index_missing(&mut s, e, 8).unwrap();
    s
}

fn dense_ids(s: &Searcher<'_>, q: &str, valid_at: i64, known_at: i64) -> Vec<String> {
    let p = SearchParams {
        lexical_weight: 0.0,
        valid_at,
        known_at,
        ..SearchParams::at(NOW)
    };
    s.candidates(q, &p)
        .unwrap()
        .dense
        .into_iter()
        .map(|(id, _)| id.0)
        .collect()
}

/// Every candidate passes, so only visibility and time decide what comes back.
fn open_gate() -> PointerGate {
    PointerGate {
        n: 10,
        min_cosine: -1.0,
        min_lexical: f32::NEG_INFINITY,
    }
}

#[test]
fn dense_side_follows_both_time_axes() {
    let e = HashEmbedder::new(256);
    let store = garden(&e);
    let s = Searcher::new(&store, Some(&e), &NoReranker);
    let q = "where does Mira live";
    assert_eq!(dense_ids(&s, q, NOW, NOW), ["f-birch"], "valid now");
    assert_eq!(
        dense_ids(&s, q, NOW - 50 * DAY, NOW),
        ["f-alder"],
        "valid fifty days ago"
    );
    assert_eq!(
        dense_ids(&s, q, NOW, NOW - 50 * DAY),
        Vec::<String>::new(),
        "known fifty days ago, valid now: the old address had ended, the new was unknown"
    );
    assert_eq!(
        dense_ids(&s, q, NOW - 50 * DAY, NOW - 50 * DAY),
        ["f-alder"]
    );
}

#[test]
fn hybrid_search_follows_the_valid_axis() {
    let e = HashEmbedder::new(256);
    let store = garden(&e);
    let s = Searcher::new(&store, Some(&e), &NoReranker);
    let now = s.search("Alder Street", &SearchParams::at(NOW)).unwrap();
    assert!(
        now.iter().all(|h| h.id != hit("f-alder")),
        "ended in the world"
    );
    let then = SearchParams {
        valid_at: NOW - 50 * DAY,
        ..SearchParams::at(NOW)
    };
    assert_eq!(
        s.search("Alder Street", &then).unwrap()[0].id,
        hit("f-alder")
    );
}

#[test]
fn private_facts_only_on_request() {
    let e = HashEmbedder::new(256);
    let store = garden(&e);
    let s = Searcher::new(&store, Some(&e), &NoReranker);
    let q = "therapist Orrin Hale";
    let public = SearchParams::at(NOW);
    let hits = s.search(q, &public).unwrap();
    assert!(hits.iter().all(|h| h.id != hit("f-therapist")), "{hits:?}");
    let ps = pointers(&s, q, &public, &open_gate()).unwrap();
    assert!(ps.iter().all(|p| p.id != hit("f-therapist")), "{ps:?}");

    let private = SearchParams {
        include_private: true,
        ..public
    };
    assert_eq!(s.search(q, &private).unwrap()[0].id, hit("f-therapist"));
    let ps = pointers(&s, q, &private, &PointerGate::default()).unwrap();
    assert_eq!(ps[0].id, hit("f-therapist"));
    assert!(
        !ps[0].line.contains("Orrin"),
        "a pointer names the entity, not the text"
    );
}

#[test]
fn secret_facts_never_come_back() {
    let e = HashEmbedder::new(256);
    let store = garden(&e);
    let s = Searcher::new(&store, Some(&e), &NoReranker);
    let p = SearchParams {
        include_private: true,
        k: 10,
        ..SearchParams::at(NOW)
    };
    for q in ["bank card PIN", "PIN vault", "Mira Velde"] {
        let hits = s.search(q, &p).unwrap();
        assert!(hits.iter().all(|h| h.id != hit("f-pin")), "{q}: {hits:?}");
        let ps = pointers(&s, q, &p, &open_gate()).unwrap();
        assert!(ps.iter().all(|x| x.id != hit("f-pin")), "{q}: {ps:?}");
    }
}

#[test]
fn trivial_prompts_get_no_pointers() {
    let e = HashEmbedder::new(256);
    let store = garden(&e);
    let s = Searcher::new(&store, Some(&e), &NoReranker);
    for q in ["ok", "Thanks!", "спасибо", "/new", "  "] {
        assert!(
            pointers(&s, q, &SearchParams::at(NOW), &open_gate())
                .unwrap()
                .is_empty(),
            "{q:?}"
        );
    }
}

#[test]
fn invalid_weights_are_rejected_not_ignored() {
    let e = HashEmbedder::new(256);
    let store = garden(&e);
    let s = Searcher::new(&store, Some(&e), &NoReranker);
    for dense_weight in [f32::NAN, -1.0] {
        let p = SearchParams {
            dense_weight,
            ..SearchParams::at(NOW)
        };
        assert!(matches!(
            s.search("Birch Road", &p),
            Err(SearchError::Invalid(_))
        ));
        assert!(matches!(
            pointers(&s, "Birch Road", &p, &PointerGate::default()),
            Err(SearchError::Invalid(_))
        ));
    }
}

/// A candidate of the day without an entity.
fn lifted(fact: &str) -> ulei_garden::wire::c7::ProposeRequest {
    serde_json::from_value(serde_json::json!({
        "fact": fact,
        "source_ref": "archive:7",
        "source_kind": "raw",
    }))
    .unwrap()
}

#[test]
fn records_and_facts_share_the_candidate_cut_in_one_lexical_order() {
    let e = HashEmbedder::new(256);
    let mut store = garden(&e);
    let strong = store
        .write_seed(&lifted("Birch Road, Birch Road"), None, None, NOW - DAY)
        .unwrap();
    let weak = lifted(
        "A long note from the market that mentions Birch Road once among many other words \
         about the weather, the bakery, the tram and the harbour",
    );
    store.write_seed(&weak, None, None, NOW - DAY).unwrap();
    let s = Searcher::new(&store, None, &NoReranker);
    let p = SearchParams {
        candidates: 2,
        k: 3,
        ..SearchParams::at(NOW)
    };
    let lexical: Vec<Indexed> = s
        .candidates("Birch Road", &p)
        .unwrap()
        .lexical
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let cut = [Indexed::Seed(strong), hit("f-birch")];
    assert_eq!(
        lexical, cut,
        "a record above the fact takes one slot, the fact the other"
    );
    let hits: Vec<Indexed> = s
        .search("Birch Road", &p)
        .unwrap()
        .into_iter()
        .map(|h| h.id)
        .collect();
    assert_eq!(hits, cut);
}
