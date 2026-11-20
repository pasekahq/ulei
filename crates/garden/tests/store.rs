//! Behaviour of the garden store through its public API. All people, places and texts are
//! fictional.

#![allow(missing_docs, clippy::unwrap_used)]

use std::path::PathBuf;
use std::time::Instant;

use ulei_garden::model::{
    Attrs, Domain, Entity, EntityId, EntityKind, Fact, FactId, Hardness, PlaceAttrs, Polarity,
    PreferenceAttrs, Provenance, Sensitivity, SourceKind, TtlKind, VisitAttrs, WikidataQid,
};
use ulei_garden::store::{Store, StoreError, parse_domains, starting_domains};

const DAY: i64 = 86_400;
const NOW: i64 = 1_800_000_000;

fn entity(id: &str, kind: EntityKind, name: &str, aliases: &[&str]) -> Entity {
    Entity {
        id: EntityId(id.into()),
        kind,
        name: name.into(),
        aliases: aliases.iter().map(|a| (*a).to_owned()).collect(),
        domain: None,
        attrs: Attrs::None,
    }
}

fn fact(id: &str, e: &str, text: &str, valid_from: i64, recorded_at: i64) -> Fact {
    Fact {
        id: FactId(id.into()),
        entity: EntityId(e.into()),
        text: text.into(),
        domain: None,
        attribute: None,
        ttl: TtlKind::Personal,
        valid_from,
        valid_to: None,
        recorded_at,
        superseded_at: None,
        provenance: Provenance {
            kind: SourceKind::Owner,
            source_ref: format!("msg:{id}"),
            observed_at: recorded_at,
        },
        expires_at: None,
        sensitivity: Sensitivity::Public,
    }
}

fn address(id: &str, text: &str, valid_from: i64, recorded_at: i64) -> Fact {
    Fact {
        attribute: Some("address".into()),
        ..fact(id, "p-mira", text, valid_from, recorded_at)
    }
}

fn store() -> Store {
    let mut s = Store::open_in_memory().unwrap();
    s.upsert_entity(&entity(
        "p-mira",
        EntityKind::Person,
        "Mira Velde",
        &["Mirka", "Мира"],
    ))
    .unwrap();
    s
}

fn ids(v: &[Fact]) -> Vec<&str> {
    v.iter().map(|f| f.id.0.as_str()).collect()
}

fn hit_ids(v: &[(FactId, f32)]) -> Vec<&str> {
    v.iter().map(|(f, _)| f.0.as_str()).collect()
}

fn invalid(r: Result<(), StoreError>) -> String {
    match r {
        Err(StoreError::Invalid(s)) => s,
        other => panic!("expected Invalid, got {other:?}"),
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("ulei-garden-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn fts5_is_compiled_in() {
    let s = Store::open_in_memory().unwrap();
    let on: i64 = s
        .conn()
        .query_row("SELECT sqlite_compileoption_used('ENABLE_FTS5')", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(on, 1);
}

#[test]
fn file_store_migrates_once_and_reopens() {
    let dir = TempDir::new("reopen");
    let path = dir.0.join("garden.db");
    {
        let mut s = Store::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap(), 1);
        s.upsert_entity(&entity("p-mira", EntityKind::Person, "Mira Velde", &[]))
            .unwrap();
        s.insert_fact(&fact("f1", "p-mira", "Mira plays the cello.", 0, NOW))
            .unwrap();
        let mode: String = s
            .conn()
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        let fk: i64 = s
            .conn()
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1);
    }
    let s = Store::open(&path).unwrap();
    assert_eq!(s.schema_version().unwrap(), 1);
    assert_eq!(
        s.fact(&FactId("f1".into())).unwrap().unwrap().text,
        "Mira plays the cello."
    );
    assert_eq!(
        hit_ids(&s.lexical("cello", 5, NOW, NOW, false).unwrap()),
        ["f1"]
    );
}

#[test]
fn newer_schema_is_refused() {
    let dir = TempDir::new("newer");
    let path = dir.0.join("garden.db");
    drop(Store::open(&path).unwrap());
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch("PRAGMA user_version = 99").unwrap();
    }
    assert!(matches!(Store::open(&path), Err(StoreError::Invalid(_))));
}

#[test]
fn entities_round_trip_and_are_found_by_alias() {
    let mut s = store();
    s.upsert_domain(&Domain {
        slug: "food".into(),
        label: "Food".into(),
        wikidata: Some(WikidataQid::parse("Q2095").unwrap()),
        parent: None,
    })
    .unwrap();
    let cafe = Entity {
        attrs: Attrs::Place(PlaceAttrs {
            osm_id: Some("node/1".into()),
            lat_lon: Some((10.5, -3.25)),
        }),
        domain: Some("food".into()),
        ..entity("pl-cafe", EntityKind::Place, "Cafe Lumen", &["Lumen"])
    };
    let pref = Entity {
        attrs: Attrs::Preference(PreferenceAttrs {
            holder: EntityId("p-mira".into()),
            object: EntityId("pl-cafe".into()),
            polarity: Polarity::Dislike,
            weight: 0.5,
            hardness: Hardness::Soft,
        }),
        ..entity("pr-1", EntityKind::Preference, "Mira and Cafe Lumen", &[])
    };
    let visit = Entity {
        attrs: Attrs::Visit(VisitAttrs {
            place: EntityId("pl-cafe".into()),
            at: NOW,
            rating: Some(4),
            with: vec![EntityId("p-mira".into())],
        }),
        ..entity("v-1", EntityKind::Visit, "Lunch at Lumen", &[])
    };
    for e in [&cafe, &pref, &visit] {
        s.upsert_entity(e).unwrap();
        assert_eq!(s.entity(&e.id).unwrap().as_ref(), Some(e));
    }
    let found = s.find_entities("  мира ").unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].name, "Mira Velde");
    assert_eq!(s.find_entities("LUMEN").unwrap()[0].id.0, "pl-cafe");
    assert_eq!(s.find_entities("nobody").unwrap().len(), 0);
    assert_eq!(s.domains().unwrap().len(), 1);

    let bad = Entity {
        attrs: Attrs::Place(PlaceAttrs {
            osm_id: None,
            lat_lon: None,
        }),
        ..entity("p-x", EntityKind::Person, "X", &[])
    };
    invalid(s.upsert_entity(&bad));
    let unknown_domain = Entity {
        domain: Some("nope".into()),
        ..entity("p-y", EntityKind::Person, "Y", &[])
    };
    invalid(s.upsert_entity(&unknown_domain));
}

#[test]
fn new_address_closes_the_old_one_and_history_keeps_both() {
    let mut s = store();
    let moved = 100 * DAY;
    s.insert_fact(&address("a1", "Mira lives at 3 Linden Lane.", 0, 10 * DAY))
        .unwrap();
    s.insert_fact(&address(
        "a2",
        "Mira lives at 8 Harbour Road.",
        moved,
        120 * DAY,
    ))
    .unwrap();
    let e = EntityId("p-mira".into());
    let now = 200 * DAY;

    // Current address is the last valid one.
    assert_eq!(ids(&s.facts_of(&e, now, now).unwrap()), ["a2"]);
    // As of a time before the move, today's knowledge gives the old address (closed copy).
    let before = s.facts_of(&e, moved - 1, now).unwrap();
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].text, "Mira lives at 3 Linden Lane.");
    assert_eq!(before[0].valid_to, Some(moved));
    // What the garden believed before it learned of the move.
    assert_eq!(ids(&s.facts_of(&e, now, 110 * DAY).unwrap()), ["a1"]);
    // History keeps the original, its closed copy and the new version.
    let h = s.history(&e).unwrap();
    assert_eq!(h.len(), 3);
    assert_eq!(h[0].id.0, "a1");
    assert_eq!(h[0].superseded_at, Some(120 * DAY));
    assert_eq!(
        h[0].valid_to, None,
        "the original row is not edited except superseded_at"
    );
    assert!(h.iter().any(|f| f.id.0 == "a2"));
    // Lexical search honours the axes too.
    assert_eq!(
        hit_ids(&s.lexical("Linden", 5, now, now, false).unwrap()),
        Vec::<&str>::new()
    );
    assert_eq!(
        hit_ids(&s.lexical("Linden", 5, moved - 1, now, false).unwrap()).len(),
        1
    );
}

#[test]
fn multi_valued_facts_only_add_and_bounded_versions_split() {
    let mut s = store();
    s.insert_fact(&fact("h1", "p-mira", "Mira plays the cello.", 0, 1))
        .unwrap();
    s.insert_fact(&fact("h2", "p-mira", "Mira collects stamps.", 0, 2))
        .unwrap();
    let e = EntityId("p-mira".into());
    assert_eq!(ids(&s.facts_of(&e, 10, 10).unwrap()), ["h1", "h2"]);

    // A bounded stay inside an open address splits it into before and after.
    s.insert_fact(&address("a1", "Mira lives at 3 Linden Lane.", 0, 5))
        .unwrap();
    let stay = Fact {
        valid_to: Some(300),
        ..address("a2", "Mira lives at 1 Pine Court.", 200, 6)
    };
    s.insert_fact(&stay).unwrap();
    let at = |t| {
        s.facts_of(&e, t, 10)
            .unwrap()
            .into_iter()
            .filter(|f| f.attribute.is_some())
            .map(|f| f.text)
            .collect::<Vec<_>>()
    };
    assert_eq!(at(100), ["Mira lives at 3 Linden Lane."]);
    assert_eq!(at(250), ["Mira lives at 1 Pine Court."]);
    assert_eq!(at(400), ["Mira lives at 3 Linden Lane."]);
}

#[test]
fn supersede_retracts_on_the_transaction_axis() {
    let mut s = store();
    s.insert_fact(&fact("f1", "p-mira", "Mira plays the cello.", 0, 10))
        .unwrap();
    s.supersede(&FactId("f1".into()), 20).unwrap();
    let e = EntityId("p-mira".into());
    assert_eq!(ids(&s.facts_of(&e, 30, 15).unwrap()), ["f1"]);
    assert_eq!(s.facts_of(&e, 30, 30).unwrap().len(), 0);
    invalid(s.supersede(&FactId("f1".into()), 40));
    assert!(matches!(
        s.supersede(&FactId("nope".into()), 40),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn insert_validates() {
    let mut s = store();
    let ok = fact("f1", "p-mira", "Mira plays the cello.", 0, 10);
    let mut no_ref = ok.clone();
    no_ref.provenance.source_ref = " ".into();
    assert!(invalid(s.insert_fact(&no_ref)).contains("provenance"));
    assert!(invalid(s.insert_fact(&fact("f1", "p-ghost", "x", 0, 10))).contains("unknown entity"));
    assert!(invalid(s.insert_fact(&fact("f1", "p-mira", "  ", 0, 10))).contains("empty text"));
    assert!(invalid(s.insert_fact(&fact("", "p-mira", "x", 0, 10))).contains("bad fact id"));
    let inverted = Fact {
        valid_to: Some(0),
        ..ok.clone()
    };
    assert!(invalid(s.insert_fact(&inverted)).contains("valid_to"));
    let never = Fact {
        ttl: TtlKind::NeverCache,
        ..ok.clone()
    };
    assert!(invalid(s.insert_fact(&never)).contains("never cached"));
    let unknown_domain = Fact {
        domain: Some("nope".into()),
        ..ok.clone()
    };
    assert!(invalid(s.insert_fact(&unknown_domain)).contains("unknown domain"));
    s.insert_fact(&ok).unwrap();
    assert!(invalid(s.insert_fact(&ok)).contains("already exists"));
    // An older version cannot replace a newer one.
    s.insert_fact(&address("a1", "Mira lives at 3 Linden Lane.", 0, 50))
        .unwrap();
    invalid(s.insert_fact(&address("a0", "Mira lives at 8 Harbour Road.", 10, 40)));
    // Batch insert is all or nothing.
    let batch = [
        fact("b1", "p-mira", "Mira likes rain.", 0, 60),
        fact("b2", "p-ghost", "Nobody.", 0, 60),
    ];
    invalid(s.insert_facts(&batch));
    assert!(s.fact(&FactId("b1".into())).unwrap().is_none());
}

#[test]
fn private_facts_are_gated() {
    let mut s = store();
    let private = Fact {
        sensitivity: Sensitivity::Private,
        ..fact("p1", "p-mira", "Mira sees a cardiologist on Fridays.", 0, 1)
    };
    s.insert_fact(&private).unwrap();
    s.put_vector(&FactId("p1".into()), "m", &[1.0, 0.0])
        .unwrap();
    assert_eq!(
        s.lexical("cardiologist", 5, 10, 10, false).unwrap().len(),
        0
    );
    assert_eq!(
        hit_ids(&s.lexical("cardiologist", 5, 10, 10, true).unwrap()),
        ["p1"]
    );
    assert_eq!(s.vectors("m", 10, 10, false).unwrap().len(), 0);
    assert_eq!(s.vectors("m", 10, 10, true).unwrap().len(), 1);
}

#[test]
fn secrets_stay_out_of_fts_and_vectors() {
    let mut s = store();
    let secret = Fact {
        sensitivity: Sensitivity::Secret {
            vault_ref: "vault:mira/passport".into(),
        },
        ..fact(
            "s1",
            "p-mira",
            "Mira's passport exists and is in the vault.",
            0,
            1,
        )
    };
    s.insert_fact(&secret).unwrap();
    for q in ["passport", "vault", "Mira"] {
        assert_eq!(s.lexical(q, 5, 10, 10, true).unwrap().len(), 0, "{q}");
    }
    invalid(s.put_vector(&FactId("s1".into()), "m", &[1.0]));
    assert_eq!(s.missing_vectors("m").unwrap().len(), 0);
    assert_eq!(s.vectors("m", 10, 10, true).unwrap().len(), 0);
    let fts_rows: i64 = s
        .conn()
        .query_row(
            "SELECT count(*) FROM fact_fts WHERE fact_fts MATCH 'passport'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fts_rows, 0);
    // Reached only through the entity.
    let e = &s.find_entities("Mirka").unwrap()[0];
    let facts = s.facts_of(&e.id, 10, 10).unwrap();
    assert_eq!(ids(&facts), ["s1"]);
    assert_eq!(
        facts[0].sensitivity,
        Sensitivity::Secret {
            vault_ref: "vault:mira/passport".into()
        }
    );
    // Renaming the entity does not leak the secret into the index.
    s.upsert_entity(&entity("p-mira", EntityKind::Person, "Mira V.", &[]))
        .unwrap();
    assert_eq!(s.lexical("passport", 5, 10, 10, true).unwrap().len(), 0);

    let no_ref = Fact {
        sensitivity: Sensitivity::Secret {
            vault_ref: String::new(),
        },
        ..fact("s2", "p-mira", "Mira has a bank PIN.", 0, 1)
    };
    assert!(invalid(s.insert_fact(&no_ref)).contains("vault"));
    let value = Fact {
        sensitivity: Sensitivity::Secret {
            vault_ref: "vault:pin".into(),
        },
        ..fact("s3", "p-mira", "Mira's bank PIN is 4821.", 0, 1)
    };
    assert!(invalid(s.insert_fact(&value)).contains("value"));
}

#[test]
fn internet_facts_expire_into_rechecks() {
    let mut s = store();
    s.upsert_entity(&entity("pl-cafe", EntityKind::Place, "Cafe Lumen", &[]))
        .unwrap();
    let hours = Fact {
        ttl: TtlKind::OpeningHours,
        provenance: Provenance {
            kind: SourceKind::Internet,
            source_ref: "https://example.org/lumen".into(),
            observed_at: NOW,
        },
        ..fact("o1", "pl-cafe", "Cafe Lumen opens at 8:00.", NOW, NOW)
    };
    s.insert_fact(&hours).unwrap();
    let stored = s.fact(&FactId("o1".into())).unwrap().unwrap();
    assert_eq!(
        stored.expires_at,
        Some(NOW + 30 * DAY),
        "filled from the TTL table"
    );
    let weather = Fact {
        ttl: TtlKind::Weather,
        ..fact("w1", "pl-cafe", "Sunny near Cafe Lumen.", NOW, NOW)
    };
    s.insert_fact(&weather).unwrap();
    let e = EntityId("pl-cafe".into());

    assert_eq!(s.expired(NOW + 2 * 3600).unwrap().len(), 0);
    let later = NOW + 4 * 3600;
    assert_eq!(ids(&s.expired(later).unwrap()), ["w1"]);
    assert_eq!(ids(&s.facts_of(&e, later, later).unwrap()), ["o1"]);
    let much_later = NOW + 31 * DAY;
    assert_eq!(ids(&s.expired(much_later).unwrap()), ["w1", "o1"]);

    let marked = s.mark_expired(much_later).unwrap();
    assert_eq!(marked.len(), 2);
    assert_eq!(s.expired(much_later).unwrap().len(), 0);
    assert_eq!(ids(&s.rechecks().unwrap()), ["o1", "w1"]);
    // Not current any more, but the past is still answerable.
    assert_eq!(s.facts_of(&e, much_later, much_later).unwrap().len(), 0);
    let past = s.facts_of(&e, NOW + DAY, much_later).unwrap();
    assert_eq!(past.len(), 1);
    assert_eq!(past[0].text, "Cafe Lumen opens at 8:00.");
    assert_eq!(past[0].valid_to, Some(NOW + 30 * DAY));
    // Nothing is deleted.
    assert_eq!(s.history(&e).unwrap().len(), 4);
    s.clear_recheck(&FactId("w1".into())).unwrap();
    assert_eq!(ids(&s.rechecks().unwrap()), ["o1"]);
    assert!(matches!(
        s.clear_recheck(&FactId("w1".into())),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn vectors_round_trip_and_follow_closed_copies() {
    let mut s = store();
    s.insert_fact(&address("a1", "Mira lives at 3 Linden Lane.", 0, 10))
        .unwrap();
    s.insert_fact(&fact("h1", "p-mira", "Mira plays the cello.", 0, 10))
        .unwrap();
    let missing = s.missing_vectors("bge").unwrap();
    assert_eq!(
        missing,
        [
            (
                FactId("a1".into()),
                "Mira Velde: Mira lives at 3 Linden Lane.".to_owned()
            ),
            (
                FactId("h1".into()),
                "Mira Velde: Mira plays the cello.".to_owned()
            ),
        ]
    );
    let v = [0.25_f32, -1.5, 3.0];
    s.put_vector(&FactId("a1".into()), "bge", &v).unwrap();
    s.put_vector(&FactId("h1".into()), "bge", &[0.0, 1.0, 0.0])
        .unwrap();
    assert_eq!(
        s.vectors("bge", 20, 20, false).unwrap()[0],
        (FactId("a1".into()), v.to_vec())
    );
    assert_eq!(s.vectors("other", 20, 20, true).unwrap().len(), 0);
    assert_eq!(s.missing_vectors("other").unwrap().len(), 2);
    invalid(s.put_vector(&FactId("h1".into()), "bge", &[1.0]));
    invalid(s.put_vector(&FactId("h1".into()), "bge", &[f32::NAN, 0.0, 0.0]));
    assert!(matches!(
        s.put_vector(&FactId("x".into()), "bge", &[1.0, 0.0, 0.0]),
        Err(StoreError::NotFound(_))
    ));

    // The closed copy of the old address keeps its vector; the new version needs one.
    s.insert_fact(&address("a2", "Mira lives at 8 Harbour Road.", 100, 200))
        .unwrap();
    let missing: Vec<_> = s
        .missing_vectors("bge")
        .unwrap()
        .into_iter()
        .map(|m| m.0.0)
        .collect();
    assert_eq!(missing, ["a2"]);
    let old = s.vectors("bge", 50, 300, false).unwrap();
    assert_eq!(old.len(), 2);
    assert!(
        old.iter()
            .any(|(id, x)| id.0 != "a1" && id.0 != "h1" && x == &v)
    );
}

#[test]
fn lexical_ranks_english_and_russian() {
    let mut s = store();
    s.upsert_entity(&entity(
        "pl-dent",
        EntityKind::Place,
        "Clinic Aurora",
        &["Аврора"],
    ))
    .unwrap();
    let rows = [
        ("e1", "p-mira", "Mira's dentist is at Clinic Aurora."),
        ("e2", "p-mira", "Mira walks the dog every morning."),
        ("e3", "pl-dent", "Clinic Aurora is open on Saturdays."),
        ("r1", "p-mira", "Мира ходит к стоматологу в клинику Аврора."),
        ("r2", "p-mira", "Мира любит пирог с вишней."),
        (
            "r3",
            "p-mira",
            "Мира играет на виолончели, вишнёвый сад ей снится.",
        ),
    ];
    for (id, e, t) in rows {
        s.insert_fact(&fact(id, e, t, 0, 1)).unwrap();
    }
    let top = |q| {
        hit_ids(&s.lexical(q, 3, 10, 10, false).unwrap())
            .first()
            .map(|x| (*x).to_owned())
    };
    assert_eq!(top("dentist").as_deref(), Some("e1"));
    assert_eq!(top("dog morning").as_deref(), Some("e2"));
    assert_eq!(
        top("стоматолог").as_deref(),
        Some("r1"),
        "prefix match covers this inflection"
    );
    assert_eq!(top("пирог").as_deref(), Some("r2"));
    // The entity's alias is searchable: facts about the clinic match its Russian name.
    let aurora = hit_ids(&s.lexical("аврора", 5, 10, 10, false).unwrap())
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert!(aurora.contains(&"e3".to_owned()) && aurora.contains(&"r1".to_owned()));
    // "вишнёвый" and "вишневый" meet: the store folds ё to е, unicode61 does not.
    assert!(hit_ids(&s.lexical("вишневый", 5, 10, 10, false).unwrap()).contains(&"r3"));
    // Known gap without morphology: a different stem is not found.
    assert!(!hit_ids(&s.lexical("стоматологи", 5, 10, 10, false).unwrap()).contains(&"r1"));
    // Scores are higher for better hits, best first, and k bounds the result.
    let hits = s.lexical("Mira Aurora dentist", 2, 10, 10, false).unwrap();
    assert_eq!(hits.len(), 2);
    assert!(hits[0].1 >= hits[1].1);
    assert_eq!(s.lexical("\"*) OR (", 5, 10, 10, false).unwrap().len(), 0);
}

#[test]
fn cold_open_of_a_10k_fact_store() {
    let dir = TempDir::new("cold");
    let path = dir.0.join("garden.db");
    {
        let mut s = Store::open(&path).unwrap();
        for i in 0..100 {
            s.upsert_entity(&entity(
                &format!("p{i}"),
                EntityKind::Person,
                &format!("Person {i}"),
                &[],
            ))
            .unwrap();
        }
        let facts: Vec<Fact> = (0..10_000)
            .map(|i| {
                fact(
                    &format!("f{i}"),
                    &format!("p{}", i % 100),
                    &format!("Person {} owns item number {i} in the garden.", i % 100),
                    0,
                    1,
                )
            })
            .collect();
        s.insert_facts(&facts).unwrap();
    }
    let t = Instant::now();
    let s = Store::open(&path).unwrap();
    let hits = s.lexical("item garden", 3, 10, 10, false).unwrap();
    let elapsed = t.elapsed();
    assert_eq!(hits.len(), 3);
    eprintln!("cold open + first lexical query of 10k facts: {elapsed:?}");
    assert!(elapsed.as_secs() < 5, "{elapsed:?}");
}

#[test]
fn starting_domains_seed_once() {
    let ds = starting_domains().unwrap();
    assert_eq!(ds.len(), 20);
    assert!(ds.iter().all(|d| d.wikidata.is_some()));
    let mut s = Store::open_in_memory().unwrap();
    assert_eq!(s.seed_domains(&ds).unwrap(), 20);
    assert_eq!(s.seed_domains(&ds).unwrap(), 0, "seeding is idempotent");
    let doctors = s.domain("doctors").unwrap().unwrap();
    assert_eq!(doctors.parent.as_deref(), Some("health"));
    assert_eq!(doctors.wikidata.unwrap().as_str(), "Q39631");
    // A renamed domain survives a re-seed.
    s.upsert_domain(&Domain {
        label: "Cuisine".into(),
        ..s.domain("food").unwrap().unwrap()
    })
    .unwrap();
    s.seed_domains(&ds).unwrap();
    assert_eq!(s.domain("food").unwrap().unwrap().label, "Cuisine");
    assert_eq!(s.domains().unwrap().len(), 20);

    assert!(parse_domains(r#"[{"slug": "x", "label": "X", "qid": "P31"}]"#).is_err());
    assert!(parse_domains(r#"[{"label": "X"}]"#).is_err());
    let orphan = parse_domains(r#"[{"slug": "x", "label": "X", "parent": "nope"}]"#).unwrap();
    assert!(matches!(
        s.seed_domains(&orphan),
        Err(StoreError::Invalid(_))
    ));
}

#[test]
fn backfilled_old_address_does_not_replace_the_current_one() {
    let mut s = store();
    s.insert_fact(&address("a-birch", "Mira lives at 5 Birch Lane.", 100, 200))
        .unwrap();
    // Learned later from old material: an address that started earlier.
    s.insert_fact(&address("a-oak", "Mira lives at 2 Oak Road.", 50, 300))
        .unwrap();
    let e = EntityId("p-mira".into());
    let text_at = |valid_at| {
        s.facts_of(&e, valid_at, 1000)
            .unwrap()
            .into_iter()
            .map(|f| f.text)
            .collect::<Vec<_>>()
    };
    assert_eq!(text_at(1000), ["Mira lives at 5 Birch Lane."]);
    assert_eq!(text_at(100), ["Mira lives at 5 Birch Lane."]);
    assert_eq!(text_at(70), ["Mira lives at 2 Oak Road."]);
    let oak = s.fact(&FactId("a-oak".into())).unwrap().unwrap();
    assert_eq!(oak.valid_to, Some(100), "clipped at the later version");
    let birch = s.fact(&FactId("a-birch".into())).unwrap().unwrap();
    assert_eq!(birch.superseded_at, None);
}

#[test]
fn place_coordinates_must_be_finite_and_in_range() {
    let mut s = store();
    for (lat, lon) in [
        (f64::NAN, 1.0),
        (1.0, f64::INFINITY),
        (500.0, 0.0),
        (-90.5, 0.0),
        (0.0, 180.5),
        (0.0, -181.0),
    ] {
        let pl = Entity {
            attrs: Attrs::Place(PlaceAttrs {
                osm_id: None,
                lat_lon: Some((lat, lon)),
            }),
            ..entity("pl", EntityKind::Place, "N pl", &[])
        };
        assert!(
            invalid(s.upsert_entity(&pl)).contains("lat_lon"),
            "{lat} {lon}"
        );
        assert!(s.entity(&pl.id).unwrap().is_none());
    }
    let edge = Entity {
        attrs: Attrs::Place(PlaceAttrs {
            osm_id: None,
            lat_lon: Some((-90.0, 180.0)),
        }),
        ..entity("pl", EntityKind::Place, "N pl", &[])
    };
    s.upsert_entity(&edge).unwrap();
    assert_eq!(s.entity(&edge.id).unwrap().as_ref(), Some(&edge));
}
