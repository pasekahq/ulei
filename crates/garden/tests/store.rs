//! Behaviour of the garden store through its public API. All people, places and texts are
//! fictional.

#![allow(missing_docs, clippy::unwrap_used)]

use std::path::PathBuf;
use std::time::Instant;

use ulei_garden::edge::{Edge, EdgeConfig};
use ulei_garden::model::{
    Attrs, Domain, Entity, EntityId, EntityKind, Fact, FactId, Hardness, PlaceAttrs, Polarity,
    PreferenceAttrs, Provenance, Sensitivity, SourceKind, Ttl, TtlKind, TtlRule, TtlTable,
    VisitAttrs, WikidataQid,
};
use ulei_garden::store::{
    ForgetRef, Indexed, ModelChecks, SeedRecord, Store, StoreError, parse_domains, starting_domains,
};
use ulei_garden::wire::c7;

const HOUR: i64 = 3_600;
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

/// The tables of the last migration, the places bed: a test that turns a current store into
/// one of an older version drops them first.
const BED_TABLES: [&str; 8] = [
    "bed_dup_pair",
    "bed_recheck",
    "bed_profile",
    "bed_verdict",
    "bed_attr_history",
    "bed_attr",
    "bed_entity",
    "attr_policy",
];

/// Takes a store back to schema 16: the night judge's tables and queue state go.
fn drop_judge(s: &Store) {
    s.conn()
        .execute_batch(
            "DROP TABLE reconcile_hint;
             DROP TABLE reconcile_judged;
             DROP TABLE reconcile_judgement;
             DROP TABLE reconcile_kept;
             DROP TABLE reconcile_stats;
             ALTER TABLE reconcile_queue DROP COLUMN state;",
        )
        .unwrap();
}

fn drop_bed(s: &Store) {
    for t in BED_TABLES {
        s.conn().execute_batch(&format!("DROP TABLE {t};")).unwrap();
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

fn hit_ids(v: &[(Indexed, f32)]) -> Vec<&str> {
    v.iter()
        .map(|(hit, _)| match hit {
            Indexed::Fact(f) => f.0.as_str(),
            Indexed::Seed(seq) => panic!("record {seq} among the facts"),
        })
        .collect()
}

fn invalid<T: std::fmt::Debug>(r: Result<T, StoreError>) -> String {
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
fn file_store_bounds_its_memory() {
    let dir = TempDir::new("bound");
    let s = Store::open(&dir.0.join("garden.db")).unwrap();
    let pragma = |name: &str| -> i64 {
        s.conn()
            .query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(pragma("cache_size"), -256);
    assert_eq!(pragma("mmap_size"), 0);
    assert_eq!(pragma("temp_store"), 1);
}

#[test]
fn file_store_migrates_once_and_reopens() {
    let dir = TempDir::new("reopen");
    let path = dir.0.join("garden.db");
    {
        let mut s = Store::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap(), 17);
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
    assert_eq!(s.schema_version().unwrap(), 17);
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
fn version_10_store_refills_its_lexical_index_with_stems_and_transliteration() {
    let dir = TempDir::new("v10");
    let path = dir.0.join("garden.db");
    {
        let mut s = Store::open(&path).unwrap();
        s.upsert_entity(&entity(
            "p-orsa",
            EntityKind::Person,
            "Орса Линд",
            &["Орсик"],
        ))
        .unwrap();
        s.insert_fact(&fact("f1", "p-orsa", "Орса ходит к стоматологу.", 0, NOW))
            .unwrap();
        s.insert_fact(&Fact {
            sensitivity: Sensitivity::Secret {
                vault_ref: "vault:orsa/card".into(),
            },
            ..fact("s1", "p-orsa", "Карта Орсы лежит в сейфе.", 0, NOW)
        })
        .unwrap();
        // The index of a version 10 store: text and names only.
        drop_judge(&s);
        drop_bed(&s);
        s.conn()
            .execute_batch(
                "DROP TABLE fact_fts;
                 CREATE VIRTUAL TABLE fact_fts USING fts5(text, names, content = '',
                     contentless_delete = 1, tokenize = 'unicode61 remove_diacritics 2',
                     prefix = '2 3 4');
                 INSERT INTO fact_fts (rowid, text, names)
                     SELECT seq, text, 'Орса Линд Орсик' FROM facts WHERE id = 'f1';
                 ALTER TABLE edge_proposal DROP COLUMN seed;
                 DROP TABLE forget_queue;
                 DROP TABLE seed_marks;
                 DROP TABLE seedbed;
                 ALTER TABLE fact_vectors DROP COLUMN computed_at;
                 DROP TABLE model_checks;
                 PRAGMA user_version = 10;",
            )
            .unwrap();
    }
    let s = Store::open(&path).unwrap();
    assert_eq!(s.schema_version().unwrap(), 17);
    for q in ["стоматологи", "orsa", "Orsik", "Линда"] {
        assert_eq!(
            hit_ids(&s.lexical(q, 5, NOW, NOW, true).unwrap()),
            ["f1"],
            "{q}"
        );
    }
    for q in ["сейф", "karta", "sejf"] {
        assert_eq!(s.lexical(q, 5, NOW, NOW, true).unwrap(), [], "{q}");
    }
}

#[test]
fn version_15_store_keeps_its_vectors_as_computed_before_any_check_and_a_failed_check_drops_only_later_ones()
 {
    let dir = TempDir::new("v15");
    let path = dir.0.join("garden.db");
    {
        let mut s = Store::open(&path).unwrap();
        s.upsert_entity(&entity("p-mira", EntityKind::Person, "Mira Velde", &[]))
            .unwrap();
        for id in ["f1", "f2", "f3", "f4"] {
            s.insert_fact(&fact(id, "p-mira", &format!("Mira fact {id}."), 0, NOW))
                .unwrap();
        }
        s.put_vector(&FactId("f1".into()), "m", &[1.0, 0.0], NOW)
            .unwrap();
        // A version 15 store: no time on its vectors, no checks.
        drop_judge(&s);
        s.conn()
            .execute_batch(
                "ALTER TABLE fact_vectors DROP COLUMN computed_at;
                 DROP TABLE model_checks;
                 PRAGMA user_version = 15;",
            )
            .unwrap();
    }
    let mut s = Store::open(&path).unwrap();
    assert_eq!(s.schema_version().unwrap(), 17);
    let times = |s: &Store| -> Vec<(String, i64)> {
        s.conn()
            .prepare(
                "SELECT f.id, v.computed_at FROM fact_vectors v JOIN facts f ON f.seq = v.fact
                 ORDER BY f.seq",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    assert_eq!(
        times(&s),
        [("f1".to_owned(), 0)],
        "trusted: before any check"
    );
    assert_eq!(s.model_checks().unwrap(), ModelChecks::default());

    // Computed after the migration, with no check passed yet: a failed check drops it, not f1.
    s.put_vector(&FactId("f2".into()), "m", &[0.0, 1.0], NOW + 10)
        .unwrap();
    assert_eq!(
        s.model_check_failed("model file corrupted: x", NOW + 20)
            .unwrap(),
        1
    );
    assert_eq!(times(&s), [("f1".to_owned(), 0)]);
    assert_eq!(
        s.model_checks().unwrap(),
        ModelChecks {
            passed: None,
            failed: Some((NOW + 20, "model file corrupted: x".into())),
        }
    );
    // A passed check clears the failure; only vectors computed from its start on, of any model,
    // are dropped by the next failure.
    s.model_check_passed("ab", NOW + 100).unwrap();
    assert_eq!(
        s.model_checks().unwrap(),
        ModelChecks {
            passed: Some((NOW + 100, "ab".into())),
            failed: None,
        }
    );
    s.put_vector(&FactId("f2".into()), "m", &[0.0, 1.0], NOW + 99)
        .unwrap();
    s.put_vector(&FactId("f3".into()), "m", &[0.0, 1.0], NOW + 100)
        .unwrap();
    s.put_vector(&FactId("f4".into()), "other", &[1.0], NOW + 150)
        .unwrap();
    assert_eq!(
        s.model_check_failed("model file corrupted: y", NOW + 200)
            .unwrap(),
        2
    );
    assert_eq!(
        times(&s),
        [("f1".to_owned(), 0), ("f2".to_owned(), NOW + 99)]
    );
    assert_eq!(
        s.missing_vectors("m")
            .unwrap()
            .into_iter()
            .map(|(id, _)| id.0)
            .collect::<Vec<_>>(),
        ["f3", "f4"]
    );
    assert_eq!(
        s.model_checks().unwrap(),
        ModelChecks {
            passed: Some((NOW + 100, "ab".into())),
            failed: Some((NOW + 200, "model file corrupted: y".into())),
        }
    );
}

#[test]
fn version_16_store_withdraws_only_the_open_contradictions_the_cosine_rule_wrote() {
    let dir = TempDir::new("v16");
    let path = dir.0.join("garden.db");
    let candidate = |text: &str| {
        serde_json::json!({"fact": text, "entity": "p-mira", "source_ref": "msg:1"}).to_string()
    };
    {
        let mut s = Store::open(&path).unwrap();
        s.upsert_entity(&entity("p-mira", EntityKind::Person, "Mira Velde", &[]))
            .unwrap();
        for id in ["f1", "f2", "f3"] {
            s.insert_fact(&fact(id, "p-mira", &format!("Mira fact {id}."), 0, NOW))
                .unwrap();
        }
        drop_judge(&s);
        s.conn()
            .execute(
                "INSERT INTO edge_proposal
                     (id, status, reason, candidate, conflicts_with, fact_id, created_at,
                      resolved_at)
                 VALUES (1, 'queued', 'contradiction', ?1, 'f1', 'f2', ?4, NULL),
                        (2, 'queued', 'contradiction', ?2, 'f1', NULL, ?4, NULL),
                        (3, 'queued', 'low_confidence', ?3, NULL, NULL, ?4, NULL),
                        (4, 'accepted', 'contradiction', NULL, 'f1', 'f3', ?4, ?4);
                 ",
                rusqlite::params![
                    candidate("Mira fact f2."),
                    candidate("Mira lives in Sample Town."),
                    candidate("Mira may like tea."),
                    NOW
                ],
            )
            .unwrap();
        s.conn()
            .execute_batch(
                "INSERT INTO reconcile_queue (fact, candidate) VALUES ('f3', '{}');
                 PRAGMA user_version = 16;",
            )
            .unwrap();
    }
    let mut s = Store::open(&path).unwrap();
    assert_eq!(s.schema_version().unwrap(), 17);
    let rows: Vec<(i64, String)> = s
        .conn()
        .prepare("SELECT id, status FROM edge_proposal ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows,
        [
            (1, "withdrawn".to_owned()),
            (2, "queued".to_owned()),
            (3, "queued".to_owned()),
            (4, "accepted".to_owned())
        ]
    );
    let state: String = s
        .conn()
        .query_row(
            "SELECT state FROM reconcile_queue WHERE fact = 'f3'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "waiting", "a queued fact waits for its hints");
    for id in ["f1", "f2", "f3"] {
        let f = s.fact(&FactId(id.into())).unwrap().unwrap();
        assert_eq!(f.superseded_at, None, "{id} is left as it was");
    }
    let digest = Edge::new(&mut s, EdgeConfig::default()).digest(10).unwrap();
    assert_eq!(
        digest
            .iter()
            .map(|i| i.proposal_id.as_str())
            .collect::<Vec<_>>(),
        ["p2", "p3"],
        "the attribute contradiction and the doubt stay, the withdrawn item is gone"
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
fn supersede_by_keeps_slices_with_lineage_and_vectors() {
    let mut s = store();
    s.insert_fact(&fact("f1", "p-mira", "Mira plays the cello.", 0, 10))
        .unwrap();
    s.put_vector(&FactId("f1".into()), "m", &[1.0, 0.0], 1)
        .unwrap();
    let by = FactId("f2".into());
    let empty = s.supersede_by(&FactId("f1".into()), 20, &by, &[(50, Some(50))]);
    assert!(matches!(empty, Err(StoreError::Invalid(_))));
    let copies = s
        .supersede_by(&FactId("f1".into()), 20, &by, &[(0, Some(5)), (9, None)])
        .unwrap();
    assert_eq!(copies.len(), 2);
    let e = EntityId("p-mira".into());
    assert_eq!(s.facts_of(&e, 3, 30).unwrap()[0].id, copies[0]);
    assert_eq!(s.facts_of(&e, 7, 30).unwrap().len(), 0);
    assert_eq!(s.facts_of(&e, 50, 30).unwrap()[0].id, copies[1]);
    assert_eq!(s.facts_of(&e, 7, 15).unwrap()[0].id.0, "f1");
    let old = s.fact(&FactId("f1".into())).unwrap().unwrap();
    assert_eq!((old.superseded_at, old.superseded_by), (Some(20), Some(by)));
    assert_eq!(s.vectors("m", 3, 30, false).unwrap().len(), 1);
}

/// Mira's address `a1`, open from day 0, recorded at `NOW` and expiring at `NOW + 100 days`.
fn expiring_address() -> Fact {
    Fact {
        expires_at: Some(NOW + 100 * DAY),
        ..address("a1", "Mira lives at 3 Linden Lane.", 0, NOW)
    }
}

/// The current versions of Mira's facts, as (`valid_from`, `valid_to`, `expires_at`).
fn current_spans(s: &Store) -> Vec<(i64, Option<i64>, Option<i64>)> {
    s.history(&EntityId("p-mira".into()))
        .unwrap()
        .iter()
        .filter(|f| f.superseded_at.is_none())
        .map(|f| (f.valid_from, f.valid_to, f.expires_at))
        .collect()
}

#[test]
fn a_later_bounded_version_slices_an_open_fact_into_history_without_expiry_and_a_live_slice() {
    let mut s = store();
    s.insert_facts(&[expiring_address()]).unwrap();
    let stay = address(
        "a2",
        "Mira stays at 9 Birch Court.",
        NOW - 10 * DAY,
        NOW + DAY,
    );
    s.insert_facts(&[Fact {
        valid_to: Some(NOW + 10 * DAY),
        ..stay
    }])
    .unwrap();
    let mut spans = current_spans(&s);
    spans.sort_unstable();
    assert_eq!(
        spans,
        [
            (0, Some(NOW - 10 * DAY), None),
            (NOW - 10 * DAY, Some(NOW + 10 * DAY), None),
            (NOW + 10 * DAY, None, Some(NOW + 100 * DAY)),
        ]
    );
}

#[test]
fn supersede_by_writes_history_slices_without_expiry_and_live_ones_with_it() {
    let mut s = store();
    s.insert_fact(&expiring_address()).unwrap();
    let copies = s
        .supersede_by(
            &FactId("a1".into()),
            NOW + DAY,
            &FactId("a2".into()),
            &[
                (0, Some(NOW)),
                (NOW, Some(NOW + DAY)),
                (NOW + 5 * DAY, None),
            ],
        )
        .unwrap();
    let expiries: Vec<_> = copies
        .iter()
        .map(|id| s.fact(id).unwrap().unwrap().expires_at)
        .collect();
    assert_eq!(expiries, [None, None, Some(NOW + 100 * DAY)]);
}

#[test]
fn an_expiry_pass_leaves_history_copies_current_and_out_of_rechecks() {
    let mut s = store();
    s.insert_fact(&expiring_address()).unwrap();
    s.insert_fact(&Fact {
        valid_to: Some(NOW + 10 * DAY),
        ..address(
            "a2",
            "Mira stays at 9 Birch Court.",
            NOW - 10 * DAY,
            NOW + DAY,
        )
    })
    .unwrap();
    let expired = s.mark_expired(NOW + 200 * DAY).unwrap();
    assert_eq!(expired.len(), 1);
    let live = s.fact(&expired[0]).unwrap().unwrap();
    assert_eq!((live.valid_from, live.valid_to), (NOW + 10 * DAY, None));
    assert_eq!(ids(&s.rechecks(10).unwrap()), [expired[0].0.as_str()]);
    let mut spans = current_spans(&s);
    spans.sort_unstable();
    assert_eq!(
        spans,
        [
            (0, Some(NOW - 10 * DAY), None),
            (NOW - 10 * DAY, Some(NOW + 10 * DAY), None),
            (NOW + 10 * DAY, Some(NOW + 100 * DAY), None),
        ]
    );
    assert_eq!(s.mark_expired(NOW + 400 * DAY).unwrap(), []);
}

#[test]
fn version_4_store_drops_the_expiry_of_its_history_copies_only() {
    let dir = TempDir::new("v4");
    let path = dir.0.join("garden.db");
    let copies = {
        let mut s = Store::open(&path).unwrap();
        s.upsert_entity(&entity("p-mira", EntityKind::Person, "Mira Velde", &[]))
            .unwrap();
        s.insert_fact(&expiring_address()).unwrap();
        let copies = s
            .supersede_by(
                &FactId("a1".into()),
                NOW + DAY,
                &FactId("a2".into()),
                &[(0, Some(NOW)), (NOW + 5 * DAY, None)],
            )
            .unwrap();
        s.insert_fact(&Fact {
            valid_to: Some(NOW - DAY),
            expires_at: Some(NOW + 50 * DAY),
            ..fact("p1", "p-mira", "Mira sang in a choir.", 0, NOW)
        })
        .unwrap();
        // The copies as written before history dropped its expiry, in a version 4 store.
        drop_judge(&s);
        drop_bed(&s);
        s.conn()
            .execute_batch(&format!(
                "UPDATE facts SET expires_at = {} WHERE id = '{}';
                 DROP TABLE maintenance;
                 ALTER TABLE miss_outbox DROP COLUMN query;
                 ALTER TABLE edge_proposal DROP COLUMN entity_candidates;
                 DROP TABLE reconcile_queue;
                 DROP TABLE passage_failures;
                 ALTER TABLE edge_proposal DROP COLUMN seed;
                 DROP TABLE forget_queue;
                 DROP TABLE seed_marks;
                 DROP TABLE seedbed;
                 ALTER TABLE fact_vectors DROP COLUMN computed_at;
                 DROP TABLE model_checks;
                 PRAGMA user_version = 4;",
                NOW + 100 * DAY,
                copies[0].0
            ))
            .unwrap();
        copies
    };
    let s = Store::open(&path).unwrap();
    assert_eq!(s.schema_version().unwrap(), 17);
    let expiry = |id: &str| s.fact(&FactId(id.into())).unwrap().unwrap().expires_at;
    assert_eq!(expiry(&copies[0].0), None, "the history copy");
    assert_eq!(
        expiry(&copies[1].0),
        Some(NOW + 100 * DAY),
        "the live slice"
    );
    assert_eq!(expiry("a1"), Some(NOW + 100 * DAY), "the closed original");
    assert_eq!(
        expiry("p1"),
        Some(NOW + 50 * DAY),
        "a past fact, not a copy"
    );
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
    s.put_vector(&FactId("p1".into()), "m", &[1.0, 0.0], 1)
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
    invalid(s.put_vector(&FactId("s1".into()), "m", &[1.0], 1));
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
            quote: None,
            proof_count: 1,
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
    assert_eq!(ids(&s.rechecks(10).unwrap()), ["o1", "w1"]);
    // Not current any more, but the past is still answerable.
    assert_eq!(s.facts_of(&e, much_later, much_later).unwrap().len(), 0);
    let past = s.facts_of(&e, NOW + DAY, much_later).unwrap();
    assert_eq!(past.len(), 1);
    assert_eq!(past[0].text, "Cafe Lumen opens at 8:00.");
    assert_eq!(past[0].valid_to, Some(NOW + 30 * DAY));
    // Nothing is deleted.
    assert_eq!(s.history(&e).unwrap().len(), 4);
    s.clear_recheck(&FactId("w1".into())).unwrap();
    assert_eq!(ids(&s.rechecks(10).unwrap()), ["o1"]);
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
    s.put_vector(&FactId("a1".into()), "bge", &v, 1).unwrap();
    s.put_vector(&FactId("h1".into()), "bge", &[0.0, 1.0, 0.0], 1)
        .unwrap();
    assert_eq!(
        s.vectors("bge", 20, 20, false).unwrap()[0],
        (FactId("a1".into()), v.to_vec())
    );
    assert_eq!(s.vectors("other", 20, 20, true).unwrap().len(), 0);
    assert_eq!(s.missing_vectors("other").unwrap().len(), 2);
    invalid(s.put_vector(&FactId("h1".into()), "bge", &[1.0], 1));
    invalid(s.put_vector(&FactId("h1".into()), "bge", &[f32::NAN, 0.0, 0.0], 1));
    assert!(matches!(
        s.put_vector(&FactId("x".into()), "bge", &[1.0, 0.0, 0.0], 1),
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
fn missing_vectors_list_current_versions_before_closed_ones_that_ever_held() {
    let mut s = store();
    s.insert_fact(&fact("old", "p-mira", "Mira plays the cello.", 0, 10))
        .unwrap();
    s.supersede(&FactId("old".into()), 20).unwrap();
    s.insert_fact(&fact("never", "p-mira", "Mira plays the oboe.", 0, 30))
        .unwrap();
    s.supersede(&FactId("never".into()), 30).unwrap();
    s.insert_fact(&fact("now", "p-mira", "Mira plays the harp.", 0, 40))
        .unwrap();
    let ids = |s: &Store| -> Vec<String> {
        let missing = s.missing_vectors("bge").unwrap();
        missing.into_iter().map(|m| m.0.0).collect()
    };
    assert_eq!(
        ids(&s),
        ["now", "old"],
        "a version closed when recorded never held"
    );
    let first = |closed| s.missing_vector("bge", closed).unwrap().map(|m| m.0.0);
    assert_eq!(first(false).as_deref(), Some("now"));
    assert_eq!(first(true).as_deref(), Some("old"));

    s.put_vector(&FactId("old".into()), "bge", &[1.0, 0.0], 1)
        .unwrap();
    assert_eq!(ids(&s), ["now"]);
    assert_eq!(
        s.vectors("bge", 15, 15, false).unwrap(),
        [(FactId("old".into()), vec![1.0, 0.0])],
        "the closed version serves the transaction time it held at"
    );
    assert_eq!(s.vectors("bge", 50, 50, false).unwrap(), []);
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
    // Another case form meets through the stems column.
    assert!(hit_ids(&s.lexical("стоматологи", 5, 10, 10, false).unwrap()).contains(&"r1"));
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

#[test]
fn provenance_confidence_and_replacement_are_kept() {
    let mut s = store();
    let quoted = Fact {
        provenance: Provenance {
            kind: SourceKind::Raw,
            source_ref: "chat:7/msg:42".into(),
            observed_at: 5,
            quote: Some("I started the cello last spring".into()),
            proof_count: 3,
        },
        confidence: 0.6,
        ..fact("q1", "p-mira", "Mira plays the cello.", 0, 10)
    };
    s.insert_fact(&quoted).unwrap();
    assert_eq!(s.fact(&quoted.id).unwrap().as_ref(), Some(&quoted));
    let (trust, retrievals, helpful): (f64, i64, i64) = s
        .conn()
        .query_row(
            "SELECT trust_score, retrieval_count, helpful_count FROM facts WHERE id = 'q1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!((trust, retrievals, helpful), (0.5, 0, 0));

    s.insert_fact(&address("a1", "Mira lives at 3 Linden Lane.", 0, 10))
        .unwrap();
    s.insert_fact(&address("a2", "Mira lives at 8 Harbour Road.", 100, 20))
        .unwrap();
    let a1 = s.fact(&FactId("a1".into())).unwrap().unwrap();
    assert_eq!(a1.superseded_by, Some(FactId("a2".into())));
    s.supersede(&FactId("q1".into()), 30).unwrap();
    let q1 = s.fact(&FactId("q1".into())).unwrap().unwrap();
    assert_eq!((q1.superseded_at, q1.superseded_by), (Some(30), None));

    let ok = fact("f1", "p-mira", "Mira likes rain.", 0, 10);
    for c in [1.5, -0.1, f32::NAN] {
        let bad = Fact {
            confidence: c,
            ..ok.clone()
        };
        assert!(invalid(s.insert_fact(&bad)).contains("confidence"), "{c}");
    }
    let mut no_proof = ok.clone();
    no_proof.provenance.proof_count = 0;
    assert!(invalid(s.insert_fact(&no_proof)).contains("proof_count"));
    let mut empty_quote = ok.clone();
    empty_quote.provenance.quote = Some(" ".into());
    assert!(invalid(s.insert_fact(&empty_quote)).contains("quote"));
    let mut secret = Fact {
        sensitivity: Sensitivity::Secret {
            vault_ref: "vault:wifi".into(),
        },
        ..fact("s1", "p-mira", "The home wifi password exists.", 0, 10)
    };
    secret.provenance.quote = Some("the password is hunter".into());
    assert!(invalid(s.insert_fact(&secret)).contains("quote"));
}

#[test]
fn facts_expired_at_recording_are_refused() {
    let mut s = store();
    // Observed long before it was recorded: its 3 h lifetime ended before the garden saw it.
    let mut late = Fact {
        ttl: TtlKind::Weather,
        ..fact("w1", "p-mira", "Rain near Mira's home.", 0, 100_000)
    };
    late.provenance.observed_at = 0;
    assert!(invalid(s.insert_fact(&late)).contains("expired"));
    let explicit = Fact {
        expires_at: Some(5),
        ..fact("w2", "p-mira", "Rain near Mira's home.", 0, 10)
    };
    assert!(invalid(s.insert_fact(&explicit)).contains("expired"));
}

#[test]
fn mark_expired_skips_versions_recorded_after_now() {
    let mut s = store();
    s.upsert_entity(&entity("pl-cafe", EntityKind::Place, "Cafe Lumen", &[]))
        .unwrap();
    let phone = |id: &str, from, to, at| Fact {
        attribute: Some("phone".into()),
        ttl: TtlKind::Contacts,
        valid_to: to,
        ..fact(id, "pl-cafe", "Cafe Lumen answers on line one.", from, at)
    };
    s.insert_fact(&phone("t1", NOW, None, NOW)).unwrap();
    // A bounded correction recorded after t1's expiry writes closed copies of t1 recorded
    // later; the slice after it still holds then and carries t1's expiry.
    s.insert_fact(&phone(
        "t2",
        NOW + 10 * DAY,
        Some(NOW + 20 * DAY),
        NOW + 40 * DAY,
    ))
    .unwrap();
    let before = NOW + 35 * DAY;
    assert!(
        s.expired(before)
            .unwrap()
            .iter()
            .all(|f| f.recorded_at <= before)
    );
    s.mark_expired(before).unwrap();
    let after = NOW + 41 * DAY;
    let expired = s.mark_expired(after).unwrap();
    assert_eq!(
        expired.len(),
        1,
        "the slice after t2, not the history before it"
    );
    let slice = s.fact(&expired[0]).unwrap().unwrap();
    assert_eq!((slice.valid_from, slice.valid_to), (NOW + 20 * DAY, None));
}

#[test]
fn failed_batch_leaves_no_rows_and_no_index_entries() {
    let mut s = store();
    let before = s.history(&EntityId("p-mira".into())).unwrap().len();
    // b1 and b2 are written inside the transaction before b3 fails on its entity.
    let batch = [
        fact("b1", "p-mira", "Mira likes thunderstorms.", 0, 60),
        address("b2", "Mira lives at 3 Linden Lane.", 0, 60),
        fact("b3", "p-ghost", "Nobody.", 0, 60),
    ];
    assert!(invalid(s.insert_facts(&batch)).contains("unknown entity"));
    assert_eq!(s.history(&EntityId("p-mira".into())).unwrap().len(), before);
    assert!(s.fact(&FactId("b1".into())).unwrap().is_none());
    assert_eq!(s.lexical("thunderstorms", 5, 100, 100, true).unwrap(), []);
    assert_eq!(s.missing_vectors("m").unwrap(), []);
    // The same facts go in once the bad one is gone.
    s.insert_facts(&batch[..2]).unwrap();
    assert_eq!(
        hit_ids(&s.lexical("thunderstorms", 5, 100, 100, true).unwrap()),
        ["b1"]
    );
}

#[test]
fn hostile_fts_input_is_plain_text() {
    let mut s = store();
    s.insert_fact(&fact(
        "f1",
        "p-mira",
        "Mira plays the cello near the sea.",
        0,
        1,
    ))
    .unwrap();
    let long = (0..20_000)
        .map(|i| format!("w{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    for q in [
        long.as_str(),
        "NEAR(cello sea)",
        "^cello",
        "text:cello",
        "cello\0sea",
        "AND OR NOT",
        "\"*) OR (",
        "cello\" OR \"x",
        "{text names}: cello",
    ] {
        let hits = s.lexical(q, usize::MAX, 10, 10, false).unwrap();
        assert!(hits.len() <= 1, "{}", &q[..q.len().min(40)]);
    }
    assert_eq!(
        hit_ids(
            &s.lexical("NEAR(cello sea)", usize::MAX, 10, 10, false)
                .unwrap()
        ),
        ["f1"],
        "operators are searched as words"
    );
}

#[test]
fn version_1_store_migrates_and_renormalises_names() {
    let dir = TempDir::new("v1");
    let path = dir.0.join("garden.db");
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(
            "CREATE TABLE entities (id TEXT PRIMARY KEY, kind TEXT NOT NULL, name TEXT NOT NULL,
                 name_norm TEXT NOT NULL, domain TEXT, attrs TEXT) STRICT;
             CREATE TABLE entity_aliases (entity TEXT NOT NULL REFERENCES entities(id),
                 ord INTEGER NOT NULL, alias TEXT NOT NULL, alias_norm TEXT NOT NULL,
                 PRIMARY KEY (entity, ord)) STRICT;
             INSERT INTO entities VALUES ('p-mira', 'person', 'Mira  Velde', 'mira  velde',
                 NULL, NULL);
             INSERT INTO entity_aliases VALUES ('p-mira', 0, 'МИРКА  Вельде', 'МИРКА  Вельде');
             CREATE TABLE facts (seq INTEGER PRIMARY KEY, entity TEXT NOT NULL,
                 text TEXT NOT NULL, sensitivity TEXT NOT NULL, source_ref TEXT NOT NULL, observed_at INTEGER NOT NULL, valid_to INTEGER,
                 recorded_at INTEGER NOT NULL, superseded_at INTEGER, expires_at INTEGER);
             CREATE TABLE fact_vectors (model TEXT NOT NULL, fact INTEGER NOT NULL,
                 dim INTEGER NOT NULL, v BLOB NOT NULL, PRIMARY KEY (model, fact)) STRICT;
             PRAGMA user_version = 1;",
        )
        .unwrap();
    }
    let s = Store::open(&path).unwrap();
    assert_eq!(s.schema_version().unwrap(), 17);
    for name in ["mira velde", " мирка\tвельде "] {
        let found = s.find_entities(name).unwrap();
        assert_eq!(found.len(), 1, "{name}");
        assert_eq!(found[0].id.0, "p-mira");
    }
}

#[test]
fn a_failed_inner_unit_rolls_back_only_its_own_writes() {
    let mut s = Store::open_in_memory().unwrap();
    let out: Result<(), StoreError> = s.atomically(|s| {
        s.upsert_entity(&entity("p-mira", EntityKind::Person, "Mira Velde", &[]))?;
        let inner: Result<(), StoreError> = s.atomically(|s| {
            s.upsert_entity(&entity("p-oto", EntityKind::Person, "Oto Brask", &[]))?;
            Err(StoreError::Invalid("refused by the test".into()))
        });
        assert!(inner.is_err());
        Ok(())
    });
    out.unwrap();
    assert!(s.entity(&EntityId("p-mira".into())).unwrap().is_some());
    assert!(s.entity(&EntityId("p-oto".into())).unwrap().is_none());
    let failed: Result<(), StoreError> = s.atomically(|s| {
        s.upsert_entity(&entity("p-oto", EntityKind::Person, "Oto Brask", &[]))?;
        Err(StoreError::Invalid("refused by the test".into()))
    });
    assert!(failed.is_err());
    assert!(s.entity(&EntityId("p-oto".into())).unwrap().is_none());
}

#[test]
fn version_2_store_drops_proposal_rows_without_an_outcome() {
    let dir = TempDir::new("v2");
    let path = dir.0.join("garden.db");
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(
            "CREATE TABLE edge_proposal (id INTEGER PRIMARY KEY AUTOINCREMENT,
                 status TEXT NOT NULL, reason TEXT, candidate TEXT, conflicts_with TEXT,
                 fact_id TEXT, source_ref TEXT, created_at INTEGER NOT NULL,
                 resolved_at INTEGER);
             INSERT INTO edge_proposal (status, fact_id, created_at) VALUES
                 ('accepted', 'f1', 100), ('pending', NULL, 110), ('duplicate', 'f1', 120),
                 ('failed', NULL, 130), ('queued', NULL, 140), ('rejected', NULL, 150);
             CREATE TABLE facts (seq INTEGER PRIMARY KEY, entity TEXT NOT NULL,
                 text TEXT NOT NULL, sensitivity TEXT NOT NULL, source_ref TEXT NOT NULL, observed_at INTEGER NOT NULL, valid_to INTEGER,
                 recorded_at INTEGER NOT NULL, superseded_at INTEGER, expires_at INTEGER);
             CREATE TABLE fact_vectors (model TEXT NOT NULL, fact INTEGER NOT NULL,
                 dim INTEGER NOT NULL, v BLOB NOT NULL, PRIMARY KEY (model, fact)) STRICT;
             PRAGMA user_version = 2;",
        )
        .unwrap();
    }
    let s = Store::open(&path).unwrap();
    assert_eq!(s.schema_version().unwrap(), 17);
    let kept: Vec<String> = s
        .conn()
        .prepare("SELECT status FROM edge_proposal ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(kept, ["accepted", "duplicate", "queued", "rejected"]);
}

#[test]
fn a_new_entity_named_like_a_stored_one_of_its_kind_resolves_to_it() {
    let mut s = store();
    let again = entity("p-mira-2", EntityKind::Person, "  mira   VELDE ", &[]);
    assert_eq!(s.upsert_entity(&again).unwrap().0, "p-mira");
    let by_alias = entity("p-x", EntityKind::Person, "Someone Else", &["MIRKA"]);
    assert_eq!(s.upsert_entity(&by_alias).unwrap().0, "p-mira");
    for id in ["p-mira-2", "p-x"] {
        assert!(s.entity(&EntityId(id.into())).unwrap().is_none(), "{id}");
    }
    let place = entity("pl-mira", EntityKind::Place, "Mira Velde", &[]);
    assert_eq!(
        s.upsert_entity(&place).unwrap().0,
        "pl-mira",
        "another kind is no duplicate"
    );
    s.upsert_entity(&entity("p-brook", EntityKind::Person, "Mira Brook", &[]))
        .unwrap();

    let both = entity("p-new", EntityKind::Person, "Mirka", &["mira brook"]);
    assert!(invalid(s.upsert_entity(&both)).contains("match 2 entities"));
    assert!(s.entity(&both.id).unwrap().is_none());
    let renamed = entity("p-brook", EntityKind::Person, "Mira Brook", &["Мира"]);
    assert!(invalid(s.upsert_entity(&renamed)).contains("merge instead"));
    assert_eq!(
        s.entity(&renamed.id).unwrap().unwrap().aliases,
        Vec::<String>::new()
    );
}

fn visit_with(place: &str, with: &[&str]) -> Entity {
    Entity {
        attrs: Attrs::Visit(VisitAttrs {
            place: EntityId(place.into()),
            at: NOW,
            rating: Some(4),
            with: with.iter().map(|w| EntityId((*w).into())).collect(),
        }),
        ..entity("v-1", EntityKind::Visit, "Lunch at Lumen", &[])
    }
}

fn preference(holder: &str, object: &str, weight: f32) -> Entity {
    Entity {
        attrs: Attrs::Preference(PreferenceAttrs {
            holder: EntityId(holder.into()),
            object: EntityId(object.into()),
            polarity: Polarity::Like,
            weight,
            hardness: Hardness::Soft,
        }),
        ..entity("pr-1", EntityKind::Preference, "Mira likes Cafe Lumen", &[])
    }
}

#[test]
fn attribute_references_name_stored_entities_of_their_kind() {
    let mut s = store();
    s.upsert_entity(&entity("pl-cafe", EntityKind::Place, "Cafe Lumen", &[]))
        .unwrap();
    let cases = [
        (
            preference("p-ghost", "pl-cafe", 0.5),
            "holder p-ghost does not exist",
        ),
        (
            preference("pl-cafe", "pl-cafe", 0.5),
            "holder pl-cafe is a place, not a person",
        ),
        (
            preference("p-mira", "it-ghost", 0.5),
            "object it-ghost does not exist",
        ),
        (visit_with("pl-ghost", &[]), "place pl-ghost does not exist"),
        (
            visit_with("p-mira", &[]),
            "place p-mira is a person, not a place",
        ),
        (
            visit_with("pl-cafe", &["p-ghost"]),
            "with p-ghost does not exist",
        ),
        (
            visit_with("pl-cafe", &["p-mira", "pl-cafe"]),
            "with pl-cafe is a place, not a person",
        ),
    ];
    for (e, why) in cases {
        let got = invalid(s.upsert_entity(&e));
        assert!(got.contains(why), "{got}");
        assert!(s.entity(&e.id).unwrap().is_none(), "{why}");
    }
    s.upsert_entity(&preference("p-mira", "p-mira", 0.5))
        .unwrap();
    s.upsert_entity(&visit_with("pl-cafe", &["p-mira"]))
        .unwrap();
}

#[test]
fn preference_weight_and_visit_rating_out_of_range_are_refused() {
    let mut s = store();
    s.upsert_entity(&entity("pl-cafe", EntityKind::Place, "Cafe Lumen", &[]))
        .unwrap();
    for w in [-0.1, 1.5, f32::NAN] {
        let got = invalid(s.upsert_entity(&preference("p-mira", "pl-cafe", w)));
        assert!(got.contains("weight outside [0, 1]"), "{w}: {got}");
    }
    for r in [0, 6] {
        let mut v = visit_with("pl-cafe", &[]);
        if let Attrs::Visit(a) = &mut v.attrs {
            a.rating = Some(r);
        }
        assert!(
            invalid(s.upsert_entity(&v)).contains("rating outside 1..=5"),
            "{r}"
        );
    }
    assert!(s.entity(&EntityId("pr-1".into())).unwrap().is_none());
    assert!(s.entity(&EntityId("v-1".into())).unwrap().is_none());
}

#[test]
fn unknown_kind_in_a_stored_row_is_corrupt() {
    let s = store();
    s.conn()
        .execute(
            "INSERT INTO entities (id, kind, name, name_norm) VALUES ('r-1', 'robot', 'Robo', 'robo')",
            [],
        )
        .unwrap();
    assert!(matches!(
        s.entity(&EntityId("r-1".into())),
        Err(StoreError::Corrupt(_))
    ));
    assert!(matches!(
        s.find_entities("Robo"),
        Err(StoreError::Corrupt(_))
    ));
}

/// Mira, Cafe Lumen, and "M. Velde" with a cello fact, an address, a vector, a preference
/// held and a visit shared; every version recorded at 10.
fn store_with_a_double() -> Store {
    let mut s = store();
    s.upsert_entity(&entity("pl-cafe", EntityKind::Place, "Cafe Lumen", &[]))
        .unwrap();
    s.upsert_entity(&entity("p-mv", EntityKind::Person, "M. Velde", &["Velde"]))
        .unwrap();
    s.upsert_entity(&preference("p-mv", "pl-cafe", 0.5))
        .unwrap();
    s.upsert_entity(&visit_with("pl-cafe", &["p-mira", "p-mv"]))
        .unwrap();
    s.insert_facts(&[
        fact("f-cello", "p-mv", "Plays the cello.", 0, 10),
        Fact {
            attribute: Some("address".into()),
            ..fact("f-home", "p-mv", "Lives at 3 Linden Lane.", 0, 10)
        },
        fact("f-rain", "p-mira", "Mira likes rain.", 0, 10),
    ])
    .unwrap();
    s.put_vector(&FactId("f-cello".into()), "m", &[1.0, 0.0], 1)
        .unwrap();
    s
}

#[test]
fn merge_moves_names_facts_and_references_and_the_past_answers_as_before() {
    let mut s = store_with_a_double();
    let (mira, mv) = (EntityId("p-mira".into()), EntityId("p-mv".into()));
    let copies = s.merge_entities(&mv, &mira, 100).unwrap();
    assert_eq!(copies.len(), 2);

    for name in ["M. Velde", "velde", "Mirka"] {
        let found = s.find_entities(name).unwrap();
        assert_eq!(found.len(), 1, "{name}");
        assert_eq!(found[0].id, mira, "{name}");
    }
    assert_eq!(
        s.entity(&mira).unwrap().unwrap().aliases,
        ["Mirka", "Мира", "M. Velde", "Velde"]
    );
    assert_eq!(s.survivor(&mv).unwrap().unwrap().id, mira);
    assert_eq!(s.entity(&mv).unwrap().unwrap().name, "M. Velde");

    let texts = |e: &EntityId, known: i64| -> Vec<String> {
        let mut v: Vec<String> = s
            .facts_of(e, NOW, known)
            .unwrap()
            .into_iter()
            .map(|f| f.text)
            .collect();
        v.sort();
        v
    };
    assert_eq!(
        texts(&mv, 99),
        ["Lives at 3 Linden Lane.", "Plays the cello."]
    );
    assert_eq!(texts(&mira, 99), ["Mira likes rain."]);
    assert_eq!(texts(&mv, 100), Vec::<String>::new());
    assert_eq!(
        texts(&mira, 100),
        [
            "Lives at 3 Linden Lane.",
            "Mira likes rain.",
            "Plays the cello."
        ]
    );
    assert_eq!(
        hit_ids(&s.lexical("cello", 5, NOW, 99, false).unwrap()),
        ["f-cello"]
    );
    let after = s.lexical("Velde cello", 5, NOW, 100, false).unwrap();
    assert!(hit_ids(&after).contains(&"f-cello~100"), "{after:?}");
    assert!(!hit_ids(&after).contains(&"f-cello"));

    let old = s.fact(&FactId("f-cello".into())).unwrap().unwrap();
    assert_eq!(old.superseded_at, Some(100));
    let copy = old.superseded_by.unwrap();
    assert!(copies.contains(&copy));
    let copy = s.fact(&copy).unwrap().unwrap();
    assert_eq!((copy.entity, copy.recorded_at), (mira.clone(), 100));
    assert_eq!(copy.provenance, old.provenance);
    let vectors: Vec<String> = s
        .vectors("m", NOW, 100, false)
        .unwrap()
        .into_iter()
        .map(|(f, _)| f.0)
        .collect();
    assert_eq!(vectors, ["f-cello~100"]);

    let pref = s.entity(&EntityId("pr-1".into())).unwrap().unwrap();
    let Attrs::Preference(p) = pref.attrs else {
        panic!("{pref:?}")
    };
    assert_eq!(p.holder, mira);
    let visit = s.entity(&EntityId("v-1".into())).unwrap().unwrap();
    let Attrs::Visit(v) = visit.attrs else {
        panic!("{visit:?}")
    };
    assert_eq!(v.with, std::slice::from_ref(&mira));

    assert!(invalid(s.insert_fact(&fact("f-late", "p-mv", "Sings.", 0, 200))).contains("merged"));
    assert!(
        invalid(s.upsert_entity(&entity("p-mv", EntityKind::Person, "M. V.", &[])))
            .contains("merged")
    );
    assert!(invalid(s.merge_entities(&mv, &mira, 200)).contains("merged"));
}

#[test]
fn merge_of_one_entity_two_kinds_or_clashing_attributes_is_refused() {
    let mut s = store_with_a_double();
    let (mira, mv) = (EntityId("p-mira".into()), EntityId("p-mv".into()));
    assert!(invalid(s.merge_entities(&mira, &mira, 100)).contains("itself"));
    let cafe = EntityId("pl-cafe".into());
    assert!(invalid(s.merge_entities(&cafe, &mira, 100)).contains("only one kind merges"));
    assert!(matches!(
        s.merge_entities(&EntityId("p-ghost".into()), &mira, 100),
        Err(StoreError::NotFound(_))
    ));
    assert!(invalid(s.merge_entities(&mv, &mira, 5)).contains("precedes"));
    s.insert_fact(&address("a-mira", "Mira lives at 8 Harbour Road.", 50, 60))
        .unwrap();
    let got = invalid(s.merge_entities(&mv, &mira, 100));
    assert!(got.contains("f-home") && got.contains("a-mira"), "{got}");
    assert_eq!(
        s.find_entities("Velde").unwrap()[0].id,
        mv,
        "nothing changed"
    );
    assert!(
        s.fact(&FactId("f-cello".into()))
            .unwrap()
            .unwrap()
            .superseded_at
            .is_none()
    );
}

fn domain(slug: &str, parent: Option<&str>) -> Domain {
    Domain {
        slug: slug.into(),
        label: slug.to_uppercase(),
        wikidata: None,
        parent: parent.map(str::to_owned),
    }
}

#[test]
fn domain_cycles_are_refused_on_upsert_and_in_a_seed() {
    let mut s = Store::open_in_memory().unwrap();
    for d in [
        domain("a", None),
        domain("b", Some("a")),
        domain("c", Some("b")),
    ] {
        s.upsert_domain(&d).unwrap();
    }
    let got = invalid(s.upsert_domain(&domain("a", Some("c"))));
    assert!(got.contains("cycle a -> c -> b -> a"), "{got}");
    assert!(invalid(s.upsert_domain(&domain("b", Some("b")))).contains("cycle b -> b"));
    assert_eq!(s.domain("a").unwrap().unwrap().parent, None);
    assert_eq!(s.domain("b").unwrap().unwrap().parent.as_deref(), Some("a"));

    let looped = parse_domains(
        r#"[{"slug": "x", "label": "X", "parent": "y"}, {"slug": "y", "label": "Y", "parent": "x"}]"#,
    )
    .unwrap();
    assert!(invalid(s.seed_domains(&looped)).contains("domains: cycle x -> y -> x"));
    let own = parse_domains(r#"[{"slug": "z", "label": "Z", "parent": "z"}]"#).unwrap();
    assert!(invalid(s.seed_domains(&own)).contains("cycle z -> z"));
    assert_eq!(s.domains().unwrap().len(), 3);
}

#[test]
fn visits_and_preferences_sharing_a_name_stay_apart_and_keep_their_attrs() {
    let mut s = store();
    s.upsert_entity(&entity("p-jo", EntityKind::Person, "Jo Field", &[]))
        .unwrap();
    s.upsert_entity(&entity("pl-cafe", EntityKind::Place, "Cafe Lumen", &[]))
        .unwrap();
    let lunch = |id: &str, at: i64, rating: u8, with: &str| Entity {
        id: EntityId(id.into()),
        attrs: Attrs::Visit(VisitAttrs {
            place: EntityId("pl-cafe".into()),
            at,
            rating: Some(rating),
            with: vec![EntityId(with.into())],
        }),
        ..visit_with("pl-cafe", &[])
    };
    let likes = |id: &str, holder: &str, polarity, weight, hardness| Entity {
        id: EntityId(id.into()),
        attrs: Attrs::Preference(PreferenceAttrs {
            holder: EntityId(holder.into()),
            object: EntityId("pl-cafe".into()),
            polarity,
            weight,
            hardness,
        }),
        ..preference("p-mira", "pl-cafe", 0.5)
    };
    let entities = [
        lunch("v-1", NOW, 4, "p-mira"),
        lunch("v-2", NOW + DAY, 2, "p-jo"),
        likes("pr-1", "p-mira", Polarity::Like, 0.5, Hardness::Soft),
        likes("pr-2", "p-jo", Polarity::Dislike, 0.9, Hardness::Hard),
    ];
    for e in &entities {
        assert_eq!(s.upsert_entity(e).unwrap(), e.id);
    }
    for e in &entities {
        assert_eq!(s.entity(&e.id).unwrap().as_ref(), Some(e));
    }
    assert_eq!(s.find_entities("lunch at lumen").unwrap().len(), 2);

    s.upsert_entity(&entity("it-key", EntityKind::Item, "Spare Key", &[]))
        .unwrap();
    let again = entity("it-key-2", EntityKind::Item, " spare  KEY", &[]);
    assert_eq!(s.upsert_entity(&again).unwrap().0, "it-key");
}

/// Two connections to one WAL store, as the daemon's request path and its worker hold them.
fn two_writers(tag: &str) -> (TempDir, Store, Store) {
    let dir = TempDir::new(tag);
    let path = dir.0.join("garden.db");
    let mut a = Store::open(&path).unwrap();
    a.upsert_entity(&entity("p-mira", EntityKind::Person, "Mira Velde", &[]))
        .unwrap();
    a.insert_fact(&fact("f0", "p-mira", "Mira plays the cello.", 0, NOW))
        .unwrap();
    let b = Store::open(&path).unwrap();
    (dir, a, b)
}

#[test]
fn a_unit_that_reads_first_holds_the_write_lock_so_another_writer_cannot_break_its_snapshot() {
    let (_dir, mut a, mut b) = two_writers("snapshot");
    b.conn().busy_timeout(std::time::Duration::ZERO).unwrap();
    a.atomically(|a| {
        assert!(a.fact(&FactId("f0".into()))?.is_some());
        let other = b.insert_fact(&fact("f-b", "p-mira", "Mira hums.", 0, NOW + 1));
        assert_eq!(other.unwrap_err().class(), "sqlite_busy");
        a.insert_fact(&fact("f-a", "p-mira", "Mira sings.", 0, NOW + 1))
    })
    .unwrap();
    b.insert_fact(&fact("f-b", "p-mira", "Mira hums.", 0, NOW + 1))
        .unwrap();
    for id in ["f-a", "f-b"] {
        assert!(a.fact(&FactId(id.into())).unwrap().is_some(), "{id}");
    }
}

#[test]
fn a_second_writer_waits_for_a_unit_in_progress_and_both_commit() {
    let (_dir, mut a, mut b) = two_writers("wait");
    a.insert_fact(&Fact {
        expires_at: Some(NOW + 2),
        ..fact("f-w", "p-mira", "Mira is at the market.", NOW, NOW)
    })
    .unwrap();
    let (locked, started) = std::sync::mpsc::channel();
    let request = std::thread::spawn(move || {
        a.atomically(|a| {
            assert!(a.fact(&FactId("f0".into()))?.is_some());
            locked.send(()).unwrap();
            a.insert_fact(&fact("f-a", "p-mira", "Mira sings.", 0, NOW + 1))
        })
    });
    started.recv().unwrap();
    let expired = b.mark_expired(NOW + 2);
    request.join().unwrap().unwrap();
    assert_eq!(expired.unwrap(), [FactId("f-w".into())]);
    assert!(b.fact(&FactId("f-a".into())).unwrap().is_some());
}

fn candidate(text: &str, source: c7::SourceKind) -> c7::ProposeRequest {
    c7::ProposeRequest {
        fact: text.into(),
        entity: Some("Mira".into()),
        source_ref: "msg:7".into(),
        valid_from: None,
        kind: None,
        domain: None,
        attribute: None,
        valid_to: None,
        source_kind: Some(source),
        observed_at: None,
        confidence: None,
        ttl_kind: None,
        secret_ref: None,
        private: None,
        pin: None,
    }
}

#[allow(clippy::cast_precision_loss)]
fn at(t: i64) -> f64 {
    t as f64
}

fn finds_seed(s: &Store, q: &str, seq: i64, valid_at: i64, known_at: i64, private: bool) -> bool {
    s.lexical(q, 10, valid_at, known_at, private)
        .unwrap()
        .iter()
        .any(|(hit, _)| *hit == Indexed::Seed(seq))
}

fn accepted(fact: &str) -> c7::ProposeResponse {
    c7::ProposeResponse {
        proposal_id: "p1".into(),
        status: c7::ProposalStatus::Accepted,
        fact_id: Some(fact.into()),
        reason: None,
        conflicts_with: None,
    }
}

#[test]
fn a_written_record_is_found_at_once_ranked_among_facts_under_its_negative_rowid() {
    // The claim was measured on system SQLite 3.45.1; this runs on the bundled build.
    assert!(
        rusqlite::version_number() >= 3_054_001,
        "{}",
        rusqlite::version()
    );
    let mut s = store();
    s.insert_facts(&[
        fact("f-strong", "p-mira", "cello cello cello", 0, NOW - DAY),
        fact(
            "f-weak",
            "p-mira",
            "Mira paints, and an old cello stands in a corner of her large bright studio.",
            0,
            NOW - DAY,
        ),
    ])
    .unwrap();
    let c = c7::ProposeRequest {
        observed_at: Some(at(NOW - DAY)),
        ..candidate("cello cello lessons", c7::SourceKind::Owner)
    };
    let seq = s.write_seed(&c, None, None, NOW).unwrap();
    assert!(seq >= 1);
    let rowid: i64 = s
        .conn()
        .query_row(
            "SELECT rowid FROM fact_fts WHERE fact_fts MATCH 'lessons'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rowid, -seq);
    let hits = s.lexical("cello", 5, NOW, NOW, false).unwrap();
    let order: Vec<&Indexed> = hits.iter().map(|(h, _)| h).collect();
    assert_eq!(
        order,
        [
            &Indexed::Fact(FactId("f-strong".into())),
            &Indexed::Seed(seq),
            &Indexed::Fact(FactId("f-weak".into())),
        ]
    );
    assert!(hits.windows(2).all(|w| w[0].1 >= w[1].1), "{hits:?}");
    assert!(
        !finds_seed(&s, "cello", seq, NOW - 1, NOW - 1, false),
        "before its writing"
    );
    let r = s.seed(seq).unwrap().unwrap();
    assert_eq!(r.candidate, c);
    assert_eq!(
        (r.written_at, r.valid_from, r.valid_to),
        (NOW, NOW - DAY, None)
    );
    assert_eq!(r.sensitivity, Sensitivity::Public);
    assert_eq!(r.processed, None);
}

#[test]
fn a_secret_pointer_record_has_no_index_row_and_no_record_gets_a_vector() {
    let mut s = store();
    s.insert_fact(&fact("f1", "p-mira", "Mira swims on Fridays.", 0, NOW))
        .unwrap();
    let secret = c7::ProposeRequest {
        secret_ref: Some("vault:mira/locker".into()),
        ..candidate(
            "Mira keeps her locker code in the vault.",
            c7::SourceKind::Owner,
        )
    };
    let hidden = s.write_seed(&secret, None, None, NOW).unwrap();
    let open = s
        .write_seed(
            &candidate("Mira's locker is at the pool.", c7::SourceKind::Owner),
            None,
            None,
            NOW,
        )
        .unwrap();
    let rows: Vec<i64> = s
        .conn()
        .prepare("SELECT rowid FROM fact_fts WHERE fact_fts MATCH 'locker'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows, [-open]);
    assert_eq!(
        s.seed(hidden).unwrap().unwrap().sensitivity,
        Sensitivity::Secret {
            vault_ref: "vault:mira/locker".into()
        }
    );
    let missing: Vec<String> = s
        .missing_vectors("m")
        .unwrap()
        .into_iter()
        .map(|(id, _)| id.0)
        .collect();
    assert_eq!(missing, ["f1"]);
    let record = FactId(format!("seed:{open}"));
    assert!(matches!(
        s.put_vector(&record, "m", &[1.0, 0.0], 1),
        Err(StoreError::NotFound(_))
    ));
    assert_eq!(s.missing_vectors("m").unwrap().len(), 1);
}

#[test]
fn a_record_is_not_found_after_processing_expiry_or_valid_to_and_private_only_with_private() {
    let mut s = store();
    let processed = s
        .write_seed(
            &candidate("Mira bought a kayak.", c7::SourceKind::Owner),
            None,
            None,
            NOW,
        )
        .unwrap();
    s.mark_seed_processed(processed, NOW + 10, &[accepted("f9")])
        .unwrap();
    assert!(finds_seed(&s, "kayak", processed, NOW + 9, NOW + 9, false));
    assert!(!finds_seed(
        &s,
        "kayak",
        processed,
        NOW + 10,
        NOW + 10,
        false
    ));

    let weather = c7::ProposeRequest {
        ttl_kind: Some(c7::TtlKind::Weather),
        observed_at: Some(at(NOW - HOUR)),
        ..candidate("Thunderstorms over the harbour.", c7::SourceKind::Internet)
    };
    let stormy = s.write_seed(&weather, None, None, NOW).unwrap();
    assert_eq!(
        s.seed(stormy).unwrap().unwrap().expires_at,
        Some(NOW + 2 * HOUR)
    );
    let end = NOW + 2 * HOUR;
    assert!(finds_seed(
        &s,
        "thunderstorms",
        stormy,
        end - 1,
        end - 1,
        false
    ));
    assert!(!finds_seed(&s, "thunderstorms", stormy, end, end, false));

    let bounded = c7::ProposeRequest {
        valid_to: Some(at(NOW + HOUR)),
        ..candidate("Mira is at the regatta.", c7::SourceKind::Owner)
    };
    let regatta = s.write_seed(&bounded, None, None, NOW).unwrap();
    assert!(finds_seed(
        &s,
        "regatta",
        regatta,
        NOW + HOUR - 1,
        NOW + HOUR - 1,
        false
    ));
    assert!(!finds_seed(
        &s,
        "regatta",
        regatta,
        NOW + HOUR,
        NOW + HOUR,
        false
    ));

    let private = c7::ProposeRequest {
        private: Some(true),
        ..candidate("Mira sees a physiotherapist.", c7::SourceKind::Owner)
    };
    let physio = s.write_seed(&private, None, None, NOW).unwrap();
    assert!(!finds_seed(&s, "physiotherapist", physio, NOW, NOW, false));
    assert!(finds_seed(&s, "physiotherapist", physio, NOW, NOW, true));
}

#[test]
fn a_record_expires_by_the_ttl_table_with_the_edge_default_kind_of_its_source() {
    let mut s = store();
    let mut ttl = TtlTable::default();
    ttl.set(
        TtlKind::Personal,
        TtlRule {
            ttl: Ttl::Secs(DAY),
            recheck_before_acting: false,
        },
    );
    s.set_ttl_table(ttl);
    let expiry = |s: &mut Store, c: &c7::ProposeRequest| {
        let seq = s.write_seed(c, None, None, NOW).unwrap();
        s.seed(seq).unwrap().unwrap().expires_at
    };
    let owner = candidate("Mira likes figs.", c7::SourceKind::Owner);
    assert_eq!(
        expiry(&mut s, &owner),
        Some(NOW + DAY),
        "personal from the write"
    );
    let observed = c7::ProposeRequest {
        observed_at: Some(at(NOW - HOUR)),
        ..candidate("Mira likes dates.", c7::SourceKind::Raw)
    };
    assert_eq!(
        expiry(&mut s, &observed),
        Some(NOW + DAY - HOUR),
        "personal from the observation"
    );
    let agent = candidate("Figs grow in the south.", c7::SourceKind::Agent);
    assert_eq!(expiry(&mut s, &agent), None, "general never expires");
    let weather = c7::ProposeRequest {
        ttl_kind: Some(c7::TtlKind::Weather),
        ..agent.clone()
    };
    assert_eq!(
        expiry(&mut s, &weather),
        Some(NOW + 3 * HOUR),
        "its own kind"
    );
}

#[test]
fn a_record_past_its_expiry_never_cached_or_with_an_empty_interval_is_refused() {
    let mut s = store();
    let stale = c7::ProposeRequest {
        ttl_kind: Some(c7::TtlKind::Weather),
        observed_at: Some(at(NOW - 3 * HOUR)),
        ..candidate("Fog over the harbour.", c7::SourceKind::Internet)
    };
    let live = c7::ProposeRequest {
        ttl_kind: Some(c7::TtlKind::NeverCache),
        ..candidate("The bakery is open now.", c7::SourceKind::Internet)
    };
    let empty = c7::ProposeRequest {
        valid_from: Some(at(NOW)),
        valid_to: Some(at(NOW)),
        ..candidate("Mira is in town.", c7::SourceKind::Owner)
    };
    for c in [stale, live, empty] {
        assert!(
            matches!(
                s.write_seed(&c, None, None, NOW),
                Err(StoreError::Invalid(_))
            ),
            "{}",
            c.fact
        );
    }
    assert_eq!(s.unprocessed_seeds(i64::MAX, 10).unwrap(), []);
    assert_eq!(
        s.conn()
            .query_row("SELECT count(*) FROM fact_fts", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn a_record_is_processed_once_only_and_unprocessed_ones_are_read_oldest_first() {
    let mut s = store();
    let write = |s: &mut Store, text: &str, t: i64| {
        s.write_seed(&candidate(text, c7::SourceKind::Owner), None, None, t)
            .unwrap()
    };
    let later = write(&mut s, "Mira rows on Sundays.", NOW + 5);
    let first = write(&mut s, "Mira sails on Saturdays.", NOW);
    let second = write(&mut s, "Mira runs on Mondays.", NOW + 1);
    let seqs = |v: Vec<SeedRecord>| v.into_iter().map(|r| r.seq).collect::<Vec<_>>();
    assert_eq!(
        seqs(s.unprocessed_seeds(NOW + 1, 10).unwrap()),
        [first, second]
    );
    assert_eq!(
        seqs(s.unprocessed_seeds(NOW + 5, 2).unwrap()),
        [first, second]
    );

    s.mark_seed_processed(first, NOW + 10, &[accepted("f1")])
        .unwrap();
    assert!(matches!(
        s.mark_seed_processed(first, NOW + 20, &[accepted("f2")]),
        Err(StoreError::Invalid(_))
    ));
    assert_eq!(
        s.seed(first).unwrap().unwrap().processed,
        Some((NOW + 10, vec![accepted("f1")]))
    );
    assert!(matches!(
        s.mark_seed_processed(later, NOW + 4, &[]),
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        s.mark_seed_processed(later + 100, NOW + 10, &[]),
        Err(StoreError::NotFound(_))
    ));
    assert_eq!(
        seqs(s.unprocessed_seeds(NOW + 5, 10).unwrap()),
        [second, later]
    );
}

#[test]
fn unprocessed_records_after_a_seq_are_read_oldest_first_by_written_at() {
    let mut s = store();
    let write = |s: &mut Store, text: &str, t: i64| {
        s.write_seed(&candidate(text, c7::SourceKind::Owner), None, None, t)
            .unwrap()
    };
    let rows = write(&mut s, "Mira rows on Sundays.", NOW + 2);
    let sails = write(&mut s, "Mira sails on Saturdays.", NOW);
    let runs = write(&mut s, "Mira runs on Mondays.", NOW + 1);
    let swims = write(&mut s, "Mira swims on Fridays.", NOW + 9);
    let seqs = |v: Vec<SeedRecord>| v.into_iter().map(|r| r.seq).collect::<Vec<_>>();
    assert_eq!(
        seqs(s.unprocessed_seeds_after(0, NOW + 5, 10).unwrap()),
        [sails, runs, rows]
    );
    assert_eq!(
        seqs(s.unprocessed_seeds_after(rows, NOW + 5, 10).unwrap()),
        [sails, runs],
        "by seq, though written earlier"
    );
    assert_eq!(
        seqs(s.unprocessed_seeds_after(sails, NOW + 5, 1).unwrap()),
        [runs]
    );
    assert_eq!(
        seqs(s.unprocessed_seeds_after(runs, NOW + 9, 10).unwrap()),
        [swims]
    );
    s.forget(
        &[ForgetRef::Seed(swims)],
        "Not for the garden.",
        "s3-t12",
        NOW + 10,
    )
    .unwrap();
    assert_eq!(s.unprocessed_seeds_after(runs, NOW + 9, 10).unwrap(), []);
}

/// Mira's facts `f1` to `f3` said in the session journal's `c8:41`, `f3` pinned, all queued
/// for the night by a forget of that ref at `NOW + 10`.
fn forgotten_facts() -> Store {
    let mut s = store();
    for (id, text) in [
        ("f1", "Mira plays the cello."),
        ("f2", "Mira keeps bees."),
        ("f3", "Mira sings in a choir."),
    ] {
        let mut f = fact(id, "p-mira", text, 0, NOW);
        f.provenance.source_ref = "c8:41".into();
        s.insert_fact(&f).unwrap();
    }
    s.pin(&FactId("f3".into()), NOW).unwrap();
    let changed = s
        .forget(
            &[ForgetRef::Journal("c8:41".into())],
            "The owner asked.",
            "s3-t12",
            NOW + 10,
        )
        .unwrap();
    assert_eq!(changed[0].queue, 3);
    s
}

#[test]
fn waiting_forget_rows_are_read_by_id_after_a_row_and_taken_ones_are_left_out() {
    let mut s = forgotten_facts();
    let waiting = |s: &Store, after: i64, limit: usize| -> Vec<(i64, String)> {
        s.forget_waiting(after, limit)
            .unwrap()
            .into_iter()
            .map(|q| (q.id, q.fact.0))
            .collect()
    };
    let all = waiting(&s, 0, 10);
    let ids: Vec<i64> = all.iter().map(|(id, _)| *id).collect();
    assert_eq!(
        all.iter().map(|(_, f)| f.as_str()).collect::<Vec<_>>(),
        ["f1", "f2", "f3"]
    );
    assert_eq!(waiting(&s, 0, 2), all[..2]);
    assert_eq!(waiting(&s, ids[0], 10), all[1..]);
    assert_eq!(waiting(&s, ids[2], 10), []);
    let row = &s.forget_waiting(0, 1).unwrap()[0];
    assert_eq!(
        (row.r#ref.as_str(), row.marked_at, row.reason.as_str()),
        ("c8:41", NOW + 10, "The owner asked.")
    );
    assert_eq!((row.queued_at, row.taken_at), (NOW + 10, None));

    s.clear_forgotten(ids[1], NOW + 20).unwrap();
    assert_eq!(waiting(&s, 0, 10), [all[0].clone(), all[2].clone()]);
    assert_eq!(s.forget_queue().unwrap()[1].taken_at, Some(NOW + 20));
    assert!(s.forget_row_exists(ids[1]).unwrap());
    assert!(!s.forget_row_exists(ids[2] + 1).unwrap());
}

#[test]
fn clearing_a_forget_row_closes_its_current_fact_with_no_new_version_once() {
    let mut s = forgotten_facts();
    let ids: Vec<i64> = s.forget_queue().unwrap().iter().map(|q| q.id).collect();
    let rows_before: i64 = s
        .conn()
        .query_row("SELECT count(*) FROM facts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(s.pinned(NOW + 20).unwrap().len(), 1);

    let cleared = s.clear_forgotten(ids[2], NOW + 20).unwrap();
    assert_eq!((cleared.fact.0.as_str(), cleared.closed), ("f3", true));
    let f3 = s.fact(&FactId("f3".into())).unwrap().unwrap();
    assert_eq!((f3.superseded_at, f3.superseded_by), (Some(NOW + 20), None));
    let rows_after: i64 = s
        .conn()
        .query_row("SELECT count(*) FROM facts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows_after, rows_before, "a retraction writes no version");
    assert_eq!(
        hit_ids(&s.lexical("choir", 5, NOW + 20, NOW + 20, false).unwrap()),
        Vec::<&str>::new()
    );
    assert_eq!(
        hit_ids(&s.lexical("choir", 5, NOW + 15, NOW + 15, false).unwrap()),
        ["f3"]
    );
    assert_eq!(
        s.pinned(NOW + 20).unwrap(),
        [],
        "the pinned fact leaves the core"
    );
    assert_eq!(
        s.pinned(NOW + 15).unwrap().len(),
        1,
        "its pin stays for the past"
    );

    let again = s.clear_forgotten(ids[2], NOW + 30).unwrap();
    assert_eq!((again.fact.0.as_str(), again.closed), ("f3", false));
    let f3 = s.fact(&FactId("f3".into())).unwrap().unwrap();
    assert_eq!(f3.superseded_at, Some(NOW + 20));
    assert_eq!(s.forget_queue().unwrap()[2].taken_at, Some(NOW + 20));

    s.supersede(&FactId("f1".into()), NOW + 25).unwrap();
    let closed_before = s.clear_forgotten(ids[0], NOW + 30).unwrap();
    assert_eq!(
        (closed_before.fact.0.as_str(), closed_before.closed),
        ("f1", false)
    );
    let f1 = s.fact(&FactId("f1".into())).unwrap().unwrap();
    assert_eq!(f1.superseded_at, Some(NOW + 25));
    assert_eq!(s.forget_queue().unwrap()[0].taken_at, Some(NOW + 30));

    assert!(matches!(
        s.clear_forgotten(ids[2] + 1, NOW + 30),
        Err(StoreError::NotFound(_))
    ));
    s.unforget(
        &[ForgetRef::Journal("c8:41".into())],
        "s3-t14-n88",
        NOW + 40,
    )
    .unwrap();
    let left: Vec<String> = s
        .forget_queue()
        .unwrap()
        .into_iter()
        .map(|q| q.fact.0)
        .collect();
    assert_eq!(left, ["f1", "f3"], "undo keeps the rows the night took");
    assert!(
        s.fact(&FactId("f2".into()))
            .unwrap()
            .unwrap()
            .superseded_at
            .is_none()
    );
}

#[test]
fn clearing_a_forget_row_drops_the_recheck_of_its_fact() {
    let mut s = store();
    let mut f = fact("f1", "p-mira", "Mira's clinic opens at nine.", 0, NOW);
    f.provenance.source_ref = "c8:41".into();
    f.expires_at = Some(NOW + 5);
    s.insert_fact(&f).unwrap();
    s.forget(
        &[ForgetRef::Journal("c8:41".into())],
        "The owner asked.",
        "s3-t12",
        NOW + 1,
    )
    .unwrap();
    s.mark_expired(NOW + 6).unwrap();
    assert_eq!(ids(&s.rechecks(10).unwrap()), ["f1"]);
    let id = s.forget_queue().unwrap()[0].id;
    let cleared = s.clear_forgotten(id, NOW + 7).unwrap();
    assert!(!cleared.closed, "expiry closed it first");
    assert_eq!(s.rechecks(10).unwrap(), []);
}

#[test]
fn version_12_store_gains_the_seedbed() {
    let dir = TempDir::new("v12");
    let path = dir.0.join("garden.db");
    {
        let mut s = Store::open(&path).unwrap();
        s.upsert_entity(&entity("p-mira", EntityKind::Person, "Mira Velde", &[]))
            .unwrap();
        s.insert_fact(&fact("f1", "p-mira", "Mira plays the cello.", 0, NOW))
            .unwrap();
        drop_judge(&s);
        drop_bed(&s);
        s.conn()
            .execute_batch(
                "INSERT INTO edge_proposal (status, fact_id, created_at)
                     VALUES ('accepted', 'f1', 100);
                 ALTER TABLE edge_proposal DROP COLUMN seed;
                 DROP TABLE forget_queue;
                 DROP TABLE seed_marks;
                 DROP TABLE seedbed;
                 ALTER TABLE fact_vectors DROP COLUMN computed_at;
                 DROP TABLE model_checks;
                 PRAGMA user_version = 12;",
            )
            .unwrap();
    }
    let mut s = Store::open(&path).unwrap();
    assert_eq!(s.schema_version().unwrap(), 17);
    let seed: Option<i64> = s
        .conn()
        .query_row("SELECT seed FROM edge_proposal", [], |r| r.get(0))
        .unwrap();
    assert_eq!(seed, None);
    let seq = s
        .write_seed(
            &candidate("Mira tunes her cello.", c7::SourceKind::Owner),
            None,
            None,
            NOW,
        )
        .unwrap();
    let hits = s.lexical("cello", 5, NOW, NOW, false).unwrap();
    assert_eq!(hits.len(), 2, "{hits:?}");
    assert!(hits.iter().any(|(h, _)| *h == Indexed::Seed(seq)));
    assert!(
        hits.iter()
            .any(|(h, _)| *h == Indexed::Fact(FactId("f1".into())))
    );
}

/// The TTL table of type `place` (ADR-0080 section 3) as `(key, form, ttl_days, on_stale)`.
const PLACE_POLICY: [(&str, &str, Option<i64>, &str); 14] = [
    ("address", "text", Some(365), "show"),
    ("cuisine", "list", Some(365), "demote"),
    ("hours", "hours", Some(7), "distrust"),
    ("location", "point", Some(365), "show"),
    ("maps_url", "url", None, "show"),
    ("notes", "text", None, "show"),
    ("operating_status", "status", Some(14), "demote"),
    ("osm_id", "osm_id", None, "show"),
    ("price_for_two", "range", Some(30), "demote"),
    ("rating", "number", Some(30), "demote"),
    ("review_count", "count", Some(30), "demote"),
    ("verdict", "verdict", None, "show"),
    ("vibe", "list", None, "show"),
    ("warnings", "list", None, "show"),
];

fn place_policy(s: &Store) -> Vec<(String, String, Option<i64>, String)> {
    s.conn()
        .prepare(
            "SELECT key, form, ttl_days, on_stale FROM attr_policy WHERE type = 'place'
             ORDER BY key",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn expected_place_policy() -> Vec<(String, String, Option<i64>, String)> {
    PLACE_POLICY
        .iter()
        .map(|(k, f, t, o)| ((*k).to_owned(), (*f).to_owned(), *t, (*o).to_owned()))
        .collect()
}

#[test]
fn a_fresh_store_holds_the_ttl_policy_of_places() {
    let s = Store::open_in_memory().unwrap();
    assert_eq!(place_policy(&s), expected_place_policy());
}

#[test]
fn version_14_store_gains_the_places_bed_and_its_policy() {
    let dir = TempDir::new("v14");
    let path = dir.0.join("garden.db");
    {
        let mut s = Store::open(&path).unwrap();
        s.upsert_entity(&entity("p-mira", EntityKind::Person, "Mira Velde", &[]))
            .unwrap();
        s.insert_fact(&fact("f1", "p-mira", "Mira plays the cello.", 0, NOW))
            .unwrap();
        drop_judge(&s);
        drop_bed(&s);
        s.conn()
            .execute_batch(
                "ALTER TABLE fact_vectors DROP COLUMN computed_at;
                 DROP TABLE model_checks;
                 PRAGMA user_version = 14;",
            )
            .unwrap();
    }
    let s = Store::open(&path).unwrap();
    assert_eq!(s.schema_version().unwrap(), 17);
    assert_eq!(place_policy(&s), expected_place_policy());
    for t in BED_TABLES {
        let made: i64 = s
            .conn()
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [t],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(made, 1, "{t}");
    }
    assert_eq!(
        s.fact(&FactId("f1".into())).unwrap().unwrap().text,
        "Mira plays the cello."
    );
}

#[test]
fn version_13_store_gains_forget_marks_and_queue() {
    let dir = TempDir::new("v13");
    let path = dir.0.join("garden.db");
    let seq;
    {
        let mut s = Store::open(&path).unwrap();
        seq = s
            .write_seed(
                &candidate("Mira tunes her cello.", c7::SourceKind::Owner),
                None,
                None,
                NOW,
            )
            .unwrap();
        drop_judge(&s);
        drop_bed(&s);
        s.conn()
            .execute_batch(
                "DROP TABLE forget_queue;
                 DROP TABLE seed_marks;
                 ALTER TABLE fact_vectors DROP COLUMN computed_at;
                 DROP TABLE model_checks;
                 PRAGMA user_version = 13;",
            )
            .unwrap();
    }
    let mut s = Store::open(&path).unwrap();
    assert_eq!(s.schema_version().unwrap(), 17);
    assert!(!s.seed_forgotten(seq).unwrap());
    let changed = s
        .forget(
            &[ForgetRef::Seed(seq)],
            "Not for the garden.",
            "s3-t12",
            NOW + 1,
        )
        .unwrap();
    assert_eq!((changed[0].marks, changed[0].queue), (1, 0));
    assert!(s.seed_forgotten(seq).unwrap());
    assert_eq!(s.unprocessed_seeds(NOW + 5, 10).unwrap(), []);
}

#[test]
fn a_failed_migration_leaves_the_store_unchanged() {
    let dir = TempDir::new("failed");
    let path = dir.0.join("garden.db");
    {
        let s = Store::open(&path).unwrap();
        // A version 10 store whose `edge_proposal` already has the column the seedbed's
        // migration adds, so its last step fails.
        drop_judge(&s);
        s.conn()
            .execute_batch(
                "DROP TABLE fact_fts;
                 CREATE VIRTUAL TABLE fact_fts USING fts5(text, names, content = '',
                     contentless_delete = 1, tokenize = 'unicode61 remove_diacritics 2',
                     prefix = '2 3 4');
                 DROP TABLE forget_queue;
                 DROP TABLE seed_marks;
                 DROP TABLE seedbed;
                 DROP TABLE pins;
                 PRAGMA user_version = 10;",
            )
            .unwrap();
    }
    assert!(matches!(Store::open(&path), Err(StoreError::Sqlite(_))));
    let c = rusqlite::Connection::open(&path).unwrap();
    let count = |sql: &str| -> i64 { c.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(count("PRAGMA user_version"), 10);
    assert_eq!(
        count("SELECT count(*) FROM sqlite_master WHERE name IN ('seedbed', 'pins')"),
        0
    );
    assert_eq!(
        count("SELECT count(*) FROM pragma_table_info('fact_fts')"),
        2
    );
}
