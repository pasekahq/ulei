//! Behaviour of the places bed's writes through `places::Bed`, read back with plain SQL. Every
//! place, point, source and verdict is invented.

#![allow(missing_docs, clippy::unwrap_used)]

use ulei_garden::model::{Attrs, Entity, EntityId, EntityKind, PlaceAttrs};
use ulei_garden::places::{
    Bed, BedError, Form, Observation, Observed, PlaceRef, PlaceWrite, Placed, Profiled, Refusal,
    Said, Status, VerdictWrite,
};
use ulei_garden::store::Store;

const DAY: i64 = 86_400;
const T0: i64 = 1_900_000_000;
const NOW: i64 = T0 + 30 * DAY;

fn place(s: &mut Store, name: &str) -> EntityId {
    Bed::new(s)
        .place(
            &PlaceWrite {
                place: PlaceRef::Name(name),
                status: Status::Active,
                source: "import:places.md",
            },
            NOW,
        )
        .unwrap()
        .id()
        .clone()
}

fn obs<'a>(key: &'a str, value: &'a str, checked_at: i64, source: &'a str) -> Observation<'a> {
    Observation {
        key,
        value,
        checked_at: Some(checked_at),
        source,
        confidence: 1.0,
    }
}

fn observe(s: &mut Store, id: &EntityId, o: &Observation<'_>) -> Result<Observed, BedError> {
    Bed::new(s).observe(id, o, NOW)
}

fn verdict(v: i64, said_at: i64, source_ref: &str) -> VerdictWrite<'_> {
    VerdictWrite {
        verdict: v,
        note: None,
        said_at,
        source_ref,
        better_than: None,
    }
}

fn refusal(r: Result<impl std::fmt::Debug, BedError>) -> Refusal {
    match r {
        Err(BedError::Refused(r)) => r,
        other => panic!("not refused: {other:?}"),
    }
}

fn count(s: &Store, sql: &str) -> i64 {
    s.conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

/// Rows of every table a bed write touches.
fn rows(s: &Store) -> Vec<i64> {
    [
        "entities",
        "bed_entity",
        "bed_attr",
        "bed_attr_history",
        "bed_verdict",
        "bed_profile",
    ]
    .iter()
    .map(|t| count(s, &format!("SELECT count(*) FROM {t}")))
    .collect()
}

/// `(value, num, checked_at, source, confidence, due_at)` of the current row of `key`.
type AttrRow = (String, Option<f64>, i64, String, f64, Option<i64>);

fn attr(s: &Store, id: &EntityId, key: &str) -> AttrRow {
    s.conn()
        .query_row(
            "SELECT value, num, checked_at, source, confidence, due_at FROM bed_attr
             WHERE entity = ?1 AND key = ?2",
            [&id.0, key],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .unwrap()
}

/// `(value, checked_at, source, replaced_at)` of the history of `key`, oldest check first.
fn history(s: &Store, id: &EntityId, key: &str) -> Vec<(String, i64, String, i64)> {
    s.conn()
        .prepare(
            "SELECT value, checked_at, source, replaced_at FROM bed_attr_history
             WHERE entity = ?1 AND key = ?2 ORDER BY checked_at, seq",
        )
        .unwrap()
        .query_map([&id.0, key], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn a_place_by_name_gets_its_entity_and_bed_row_and_is_found_by_id() {
    let mut s = Store::open_in_memory().unwrap();
    let mut bed = Bed::new(&mut s);
    let made = bed
        .place(
            &PlaceWrite {
                place: PlaceRef::Name("  Lantern Noodle Bar "),
                status: Status::Candidate,
                source: "fixture-delivery-app",
            },
            T0,
        )
        .unwrap();
    let Placed::Created(id) = made else {
        panic!("{made:?}")
    };
    let again = bed
        .place(
            &PlaceWrite {
                place: PlaceRef::Id(&id),
                status: Status::Active,
                source: "owner",
            },
            T0 + DAY,
        )
        .unwrap();
    assert_eq!(again, Placed::Found(id.clone()));
    let e = s.entity(&id).unwrap().unwrap();
    assert_eq!(
        (e.kind, e.name.as_str()),
        (EntityKind::Place, "Lantern Noodle Bar")
    );
    let row: (String, String, String, i64, i64) = s
        .conn()
        .query_row(
            "SELECT type, status, source, created_at, status_at FROM bed_entity WHERE entity = ?1",
            [&id.0],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(
        row,
        (
            "place".into(),
            "active".into(),
            "fixture-delivery-app".into(),
            T0,
            T0 + DAY
        )
    );
}

#[test]
fn a_place_entity_without_a_bed_row_is_adopted_and_a_bedded_namesake_is_not() {
    let mut s = Store::open_in_memory().unwrap();
    let known = EntityId("pl-tidepool".into());
    s.upsert_entity(&Entity {
        id: known.clone(),
        kind: EntityKind::Place,
        name: "Tidepool Kitchen".into(),
        aliases: Vec::new(),
        domain: None,
        attrs: Attrs::None,
    })
    .unwrap();
    let write = PlaceWrite {
        place: PlaceRef::Name("tidepool kitchen"),
        status: Status::Candidate,
        source: "fixture-delivery-app",
    };
    let first = Bed::new(&mut s).place(&write, NOW).unwrap();
    assert_eq!(first, Placed::Adopted(known.clone()));
    let second = Bed::new(&mut s).place(&write, NOW).unwrap();
    let Placed::Created(other) = second else {
        panic!("{second:?}")
    };
    assert_ne!(other, known);
    assert_eq!(count(&s, "SELECT count(*) FROM bed_entity"), 2);
}

#[test]
fn every_attribute_keeps_its_check_source_number_and_due_time() {
    let mut s = Store::open_in_memory().unwrap();
    let id = place(&mut s, "Lantern Noodle Bar");
    let writes = [
        ("rating", "4.5", "fixture-maps"),
        ("review_count", "412", "fixture-maps"),
        ("price_for_two", "30-40", "estimate"),
        ("hours", "Mo-Sa 12:00-22:00; Su off", "fixture-osm"),
        ("operating_status", "open", "fixture-osm"),
        ("cuisine", "noodles, dumplings", "fixture-listing"),
        ("location", "0.5041, -25.0032", "fixture-osm"),
        ("address", "Lamp Lane 7", "fixture-listing"),
        ("osm_id", "node/9000001", "fixture-osm"),
        ("maps_url", "https://maps.example/lantern", "fixture-maps"),
        ("vibe", "quiet, courtyard", "owner"),
        ("warnings", "celery in the broth", "owner"),
        ("notes", "ask for the window table", "owner"),
    ];
    for (i, (key, value, source)) in writes.iter().enumerate() {
        let at = T0 + i64::try_from(i).unwrap() * DAY;
        assert_eq!(
            observe(&mut s, &id, &obs(key, value, at, source)).unwrap(),
            Observed::Written,
            "{key}"
        );
    }
    assert_eq!(
        count(&s, "SELECT count(*) FROM bed_attr"),
        i64::try_from(writes.len()).unwrap()
    );
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM bed_attr WHERE checked_at IS NULL OR source = ''"
        ),
        0
    );
    assert_eq!(
        count(
            &s,
            "SELECT count(*) FROM bed_attr a JOIN attr_policy p USING (type, key)
             WHERE a.due_at IS NOT a.checked_at + p.ttl_days * 86400"
        ),
        0
    );
    let rating = attr(&s, &id, "rating");
    assert_eq!(rating.1, Some(4.5));
    assert_eq!(rating.5, Some(T0 + 30 * DAY));
    assert_eq!(attr(&s, &id, "review_count").1, Some(412.0));
    assert_eq!(attr(&s, &id, "price_for_two").1, Some(30.0));
    let hours = attr(&s, &id, "hours");
    assert_eq!((hours.1, hours.5), (None, Some(T0 + 3 * DAY + 7 * DAY)));
    for key in ["osm_id", "maps_url", "vibe", "warnings", "notes"] {
        assert_eq!(attr(&s, &id, key).5, None, "{key}");
    }
}

#[test]
fn a_newer_observation_replaces_and_an_older_one_goes_into_history_only() {
    let mut s = Store::open_in_memory().unwrap();
    let id = place(&mut s, "Lantern Noodle Bar");
    let first = obs("rating", "4.4", T0, "fixture-maps");
    assert_eq!(observe(&mut s, &id, &first).unwrap(), Observed::Written);
    assert_eq!(observe(&mut s, &id, &first).unwrap(), Observed::Unchanged);
    let newer = Observation {
        confidence: 0.8,
        ..obs("rating", "4.6", T0 + 20 * DAY, "fixture-listing")
    };
    assert_eq!(observe(&mut s, &id, &newer).unwrap(), Observed::Replaced);
    let older = obs("rating", "4.1", T0 - 10 * DAY, "fixture-old-guide");
    assert_eq!(observe(&mut s, &id, &older).unwrap(), Observed::History);
    assert_eq!(observe(&mut s, &id, &older).unwrap(), Observed::Unchanged);
    assert_eq!(
        attr(&s, &id, "rating"),
        (
            "4.6".into(),
            Some(4.6),
            T0 + 20 * DAY,
            "fixture-listing".into(),
            0.8,
            Some(T0 + 50 * DAY)
        )
    );
    assert_eq!(
        history(&s, &id, "rating"),
        [
            ("4.1".into(), T0 - 10 * DAY, "fixture-old-guide".into(), NOW),
            ("4.4".into(), T0, "fixture-maps".into(), NOW),
        ]
    );
    let same_value = obs("rating", "4.6", T0 + 25 * DAY, "fixture-listing");
    assert_eq!(
        observe(&mut s, &id, &same_value).unwrap(),
        Observed::Replaced
    );
    assert_eq!(attr(&s, &id, "rating").2, T0 + 25 * DAY);
    assert_eq!(history(&s, &id, "rating").last().unwrap().1, T0 + 20 * DAY);
}

#[test]
fn location_and_osm_id_keep_the_place_coordinates_and_entity_attrs_in_step() {
    let mut s = Store::open_in_memory().unwrap();
    let id = place(&mut s, "Lantern Noodle Bar");
    let coords = |s: &Store| -> (Option<f64>, Option<f64>) {
        s.conn()
            .query_row(
                "SELECT lat, lon FROM bed_entity WHERE entity = ?1",
                [&id.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    };
    assert_eq!(coords(&s), (None, None));
    observe(
        &mut s,
        &id,
        &obs("location", "0.5041, -25.0032", T0, "fixture-osm"),
    )
    .unwrap();
    observe(
        &mut s,
        &id,
        &obs(
            "location",
            "0.5000, -25.0000",
            T0 - DAY,
            "fixture-old-guide",
        ),
    )
    .unwrap();
    observe(&mut s, &id, &obs("osm_id", "way/77001", T0, "fixture-osm")).unwrap();
    assert_eq!(coords(&s), (Some(0.5041), Some(-25.0032)));
    assert_eq!(
        s.entity(&id).unwrap().unwrap().attrs,
        Attrs::Place(PlaceAttrs {
            osm_id: Some("way/77001".into()),
            lat_lon: Some((0.5041, -25.0032)),
        })
    );
    observe(
        &mut s,
        &id,
        &obs("location", "0.5100, -25.0100", T0 + DAY, "fixture-osm"),
    )
    .unwrap();
    assert_eq!(coords(&s), (Some(0.51), Some(-25.01)));
}

#[test]
fn verdicts_append_once_per_source_ref_and_the_latest_is_by_said_at_then_seq() {
    let mut s = Store::open_in_memory().unwrap();
    let id = place(&mut s, "Lantern Noodle Bar");
    let rival = place(&mut s, "Tidepool Kitchen");
    let mut bed = Bed::new(&mut s);
    let liked = VerdictWrite {
        note: Some("broth was excellent"),
        better_than: Some(&rival),
        ..verdict(1, T0, "chat:msg-9001")
    };
    let Said::Written(first) = bed.verdict(&id, &liked, NOW).unwrap() else {
        panic!("not written")
    };
    assert_eq!(
        bed.verdict(&id, &liked, NOW).unwrap(),
        Said::Repeated(first)
    );
    bed.verdict(&id, &verdict(-1, T0 + 2 * DAY, "chat:msg-9002"), NOW)
        .unwrap();
    bed.verdict(&id, &verdict(0, T0 + 2 * DAY, "chat:msg-9003"), NOW)
        .unwrap();
    bed.verdict(&id, &verdict(1, T0 + DAY, "chat:msg-9004"), NOW + 1)
        .unwrap();
    assert_eq!(count(&s, "SELECT count(*) FROM bed_verdict"), 4);
    let latest: (i64, String) = s
        .conn()
        .query_row(
            "SELECT verdict, source_ref FROM bed_verdict WHERE entity = ?1
             ORDER BY said_at DESC, seq DESC LIMIT 1",
            [&id.0],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(latest, (0, "chat:msg-9003".into()));
    let kept: (Option<String>, Option<String>) = s
        .conn()
        .query_row(
            "SELECT note, better_than FROM bed_verdict WHERE seq = ?1",
            [first],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        kept,
        (Some("broth was excellent".into()), Some(rival.0.clone()))
    );
}

#[test]
fn the_profile_keeps_one_row_per_key_except_ban() {
    let mut s = Store::open_in_memory().unwrap();
    let mut bed = Bed::new(&mut s);
    let w = |bed: &mut Bed<'_>, k: &str, v: &str, at: i64| bed.profile(k, v, "owner", at).unwrap();
    assert_eq!(
        w(&mut bed, "home", "0.5000, -25.0000", T0),
        Profiled::Written
    );
    assert_eq!(w(&mut bed, "min_rating", "4.2", T0), Profiled::Written);
    assert_eq!(w(&mut bed, "min_rating", "4.2", T0), Profiled::Unchanged);
    assert_eq!(
        w(&mut bed, "min_rating", "4.4", T0 + DAY),
        Profiled::Written
    );
    assert_eq!(w(&mut bed, "budget_for_two", "40", T0), Profiled::Written);
    assert_eq!(w(&mut bed, "currency", "EUR", T0), Profiled::Written);
    assert_eq!(w(&mut bed, "ban", "celery", T0), Profiled::Written);
    assert_eq!(w(&mut bed, "ban", "fennel", T0), Profiled::Written);
    assert_eq!(w(&mut bed, "ban", "celery", T0 + DAY), Profiled::Unchanged);
    let rows: Vec<(String, String)> = s
        .conn()
        .prepare("SELECT key, value FROM bed_profile ORDER BY key, value")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let want = [
        ("ban", "celery"),
        ("ban", "fennel"),
        ("budget_for_two", "40"),
        ("currency", "EUR"),
        ("home", "0.5000, -25.0000"),
        ("min_rating", "4.4"),
    ];
    assert_eq!(rows, want.map(|(k, v)| (k.to_owned(), v.to_owned())));
}

#[test]
fn hours_outside_the_subset_are_stored_as_written() {
    let mut s = Store::open_in_memory().unwrap();
    let id = place(&mut s, "Lantern Noodle Bar");
    let o = obs(
        "hours",
        "sunrise to sunset, ask the owner",
        T0,
        "fixture-listing",
    );
    assert_eq!(observe(&mut s, &id, &o).unwrap(), Observed::Written);
    assert_eq!(attr(&s, &id, "hours").0, "sunrise to sunset, ask the owner");
}

#[test]
fn values_that_do_not_fit_their_form_are_refused_and_write_nothing() {
    let mut s = Store::open_in_memory().unwrap();
    let id = place(&mut s, "Lantern Noodle Bar");
    let before = rows(&s);
    let cases = [
        ("rating", "great", Form::Number),
        ("rating", "4,5", Form::Number),
        ("review_count", "-3", Form::Count),
        ("review_count", "12.5", Form::Count),
        ("price_for_two", "40-30", Form::Range),
        ("price_for_two", "cheap", Form::Range),
        ("operating_status", "closed", Form::Status),
        ("cuisine", "noodles,,dumplings", Form::List),
        ("location", "91.0, 0.0", Form::Point),
        ("location", "0.5041 -25.0032", Form::Point),
        ("osm_id", "place/12", Form::OsmId),
        ("osm_id", "node/", Form::OsmId),
        ("maps_url", "http://maps.example/lantern", Form::Url),
        ("address", "   ", Form::Text),
    ];
    for (key, value, form) in cases {
        assert_eq!(
            refusal(observe(&mut s, &id, &obs(key, value, T0, "fixture-osm"))),
            Refusal::BadValue {
                key: key.into(),
                form
            },
            "{key} {value:?}"
        );
    }
    let mut bed = Bed::new(&mut s);
    assert_eq!(
        refusal(bed.profile("home", "next to the river", "owner", T0)),
        Refusal::BadValue {
            key: "home".into(),
            form: Form::Point
        }
    );
    assert_eq!(
        refusal(bed.verdict(&id, &verdict(2, T0, "chat:msg-9001"), NOW)),
        Refusal::BadValue {
            key: "verdict".into(),
            form: Form::Verdict
        }
    );
    assert_eq!(rows(&s), before);
}

#[test]
fn observations_without_a_key_provenance_or_safe_value_are_refused_and_write_nothing() {
    let mut s = Store::open_in_memory().unwrap();
    let id = place(&mut s, "Lantern Noodle Bar");
    let stranger = EntityId("pl-nowhere".into());
    let before = rows(&s);
    // Assembled at run time so no token-shaped literal exists in the repository.
    let tokened = format!("https://maps.example/p/{}{}", "Ab".repeat(6), "0123456789");
    let cases: [(Observation<'_>, Refusal); 7] = [
        (
            obs("smell", "fresh bread", T0, "owner"),
            Refusal::UnknownKey("smell".into()),
        ),
        (obs("rating", "4.5", T0, "  "), Refusal::EmptySource),
        (
            Observation {
                checked_at: None,
                ..obs("rating", "4.5", T0, "fixture-maps")
            },
            Refusal::MissingCheckedAt,
        ),
        (
            obs("notes", "the door code is 4821", T0, "owner"),
            Refusal::SecretValue,
        ),
        (obs("maps_url", &tokened, T0, "owner"), Refusal::SecretValue),
        (
            Observation {
                confidence: 1.5,
                ..obs("rating", "4.5", T0, "fixture-maps")
            },
            Refusal::BadConfidence,
        ),
        (obs("verdict", "1", T0, "owner"), Refusal::VerdictKey),
    ];
    for (o, want) in &cases {
        assert_eq!(refusal(observe(&mut s, &id, o)), *want, "{o:?}");
    }
    assert_eq!(
        refusal(observe(
            &mut s,
            &stranger,
            &obs("rating", "4.5", T0, "owner")
        )),
        Refusal::UnknownPlace(stranger.clone())
    );
    assert_eq!(rows(&s), before);
}

#[test]
fn verdicts_profiles_and_places_without_provenance_or_safe_text_are_refused_and_write_nothing() {
    let mut s = Store::open_in_memory().unwrap();
    let id = place(&mut s, "Lantern Noodle Bar");
    let stranger = EntityId("pl-nowhere".into());
    let before = rows(&s);
    let mut bed = Bed::new(&mut s);
    assert_eq!(
        refusal(bed.verdict(&id, &verdict(1, T0, " "), NOW)),
        Refusal::EmptySourceRef
    );
    assert_eq!(
        refusal(bed.verdict(
            &id,
            &VerdictWrite {
                note: Some("the pin is 1234"),
                ..verdict(1, T0, "chat:msg-9001")
            },
            NOW
        )),
        Refusal::SecretValue
    );
    assert_eq!(
        refusal(bed.verdict(
            &id,
            &VerdictWrite {
                better_than: Some(&stranger),
                ..verdict(1, T0, "chat:msg-9001")
            },
            NOW
        )),
        Refusal::UnknownPlace(stranger.clone())
    );
    assert_eq!(
        refusal(bed.profile("mood", "calm", "owner", T0)),
        Refusal::UnknownKey("mood".into())
    );
    assert_eq!(
        refusal(bed.profile("ban", "celery", "", T0)),
        Refusal::EmptySourceRef
    );
    assert_eq!(
        refusal(bed.profile("ban", "anything from 7788", "owner", T0)),
        Refusal::SecretValue
    );
    assert_eq!(
        refusal(bed.place(
            &PlaceWrite {
                place: PlaceRef::Name("Lantern Noodle Bar"),
                status: Status::Candidate,
                source: "",
            },
            NOW
        )),
        Refusal::EmptySource
    );
    assert_eq!(
        refusal(bed.place(
            &PlaceWrite {
                place: PlaceRef::Name("Card 4111 1111 1111 1111"),
                status: Status::Candidate,
                source: "owner",
            },
            NOW
        )),
        Refusal::SecretValue
    );
    assert_eq!(
        refusal(bed.place(
            &PlaceWrite {
                place: PlaceRef::Id(&stranger),
                status: Status::Candidate,
                source: "owner",
            },
            NOW
        )),
        Refusal::UnknownPlace(stranger.clone())
    );
    assert_eq!(rows(&s), before);
}

#[test]
fn a_person_is_not_a_place() {
    let mut s = Store::open_in_memory().unwrap();
    let mira = EntityId("p-mira".into());
    s.upsert_entity(&Entity {
        id: mira.clone(),
        kind: EntityKind::Person,
        name: "Mira Velde".into(),
        aliases: Vec::new(),
        domain: None,
        attrs: Attrs::None,
    })
    .unwrap();
    let before = rows(&s);
    let r = Bed::new(&mut s).place(
        &PlaceWrite {
            place: PlaceRef::Id(&mira),
            status: Status::Active,
            source: "owner",
        },
        NOW,
    );
    assert_eq!(refusal(r), Refusal::NotAPlace(mira));
    assert_eq!(rows(&s), before);
}
