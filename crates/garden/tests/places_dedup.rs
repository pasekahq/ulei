//! The dedup rule of a place write by name (ADR-0080 section 6) through C7 `places_write` and
//! `places_search` on an in-memory garden. Every place, point, name and `osm_id` is invented.

#![allow(missing_docs, clippy::unwrap_used)]

use serde_json::{Value, json};
use ulei_garden::api::{Garden, Hybrid};
use ulei_garden::miss::ProducerKey;
use ulei_garden::rerank::NoReranker;
use ulei_garden::store::Store;
use ulei_garden::wire::c7;

const NOW: i64 = 1_931_731_200;
const BASE: &str = "0.5000, -25.0000";
/// About 40 m north of [`BASE`].
const NEAR_BASE: &str = "0.50036, -25.0000";
/// About 2 km north of [`BASE`].
const FAR: &str = "0.51799, -25.0000";
/// About 30 m north of [`FAR`].
const NEAR_FAR: &str = "0.51826, -25.0000";

fn garden() -> Garden {
    Garden::new(
        Store::open_in_memory().unwrap(),
        Hybrid::new(None, Box::new(NoReranker)),
        ulei_garden::api::ApiConfig::default(),
        ProducerKey::from_bytes(&[0x5a; 32]).unwrap(),
    )
}

fn obs(key: &str, value: &str) -> Value {
    json!({"key": key, "value": value, "checked_at": NOW - 86_400, "source": "fixture-maps"})
}

/// A candidate write of `name` with `attrs`.
fn place(name: &str, attrs: &[Value]) -> Value {
    json!({"name": name, "status": "candidate", "source": "fixture-maps", "attrs": attrs})
}

/// A candidate write of `name` at `location` with a rating.
fn at(name: &str, location: &str) -> Value {
    place(name, &[obs("location", location), obs("rating", "4.5")])
}

fn write(g: &mut Garden, place: &Value) -> c7::PlaceWritten {
    let req: c7::PlacesWriteRequest = serde_json::from_value(json!({ "places": [place] })).unwrap();
    g.garden_places_write(&req, NOW)
        .unwrap()
        .places
        .pop()
        .unwrap()
}

/// The id of a write answered `outcome`.
fn placed(g: &mut Garden, p: &Value, outcome: c7::PlaceOutcome) -> String {
    let w = write(g, p);
    assert_eq!(w.outcome, outcome, "{w:?}");
    w.id.unwrap()
}

fn count(g: &Garden, sql: &str) -> i64 {
    g.store().conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

/// Rows of every table a place write touches.
fn rows(g: &Garden) -> Vec<i64> {
    [
        "entities",
        "bed_entity",
        "bed_attr",
        "bed_attr_history",
        "bed_verdict",
        "bed_dup_pair",
    ]
    .iter()
    .map(|t| count(g, &format!("SELECT count(*) FROM {t}")))
    .collect()
}

/// `(a, b, distance_m, settled_at)` of every `bed_dup_pair` row.
fn pairs(g: &Garden) -> Vec<(String, String, Option<i64>, Option<i64>)> {
    g.store()
        .conn()
        .prepare("SELECT a, b, distance_m, settled_at FROM bed_dup_pair ORDER BY a, b")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn ordered(a: &str, b: &str) -> (String, String) {
    if a < b {
        (a.into(), b.into())
    } else {
        (b.into(), a.into())
    }
}

/// A second write of `p` is found as `id` with every attribute unchanged and writes no row.
fn repeat_writes_nothing(g: &mut Garden, p: &Value, id: &str) {
    let before = rows(g);
    let again = write(g, p);
    assert_eq!(
        (again.outcome, again.id.as_deref()),
        (c7::PlaceOutcome::Found, Some(id))
    );
    assert!(
        again
            .attrs
            .iter()
            .all(|a| a.outcome == c7::AttrOutcome::Unchanged),
        "{again:?}"
    );
    assert_eq!(rows(g), before);
}

fn value(g: &Garden, id: &str, key: &str) -> String {
    g.store()
        .conn()
        .query_row(
            "SELECT value FROM bed_attr WHERE entity = ?1 AND key = ?2",
            [id, key],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn the_same_osm_id_under_another_name_and_far_away_is_the_same_place() {
    let mut g = garden();
    let first = place(
        "Lantern Noodle Bar",
        &[obs("osm_id", "node/9100001"), obs("location", BASE)],
    );
    let id = placed(&mut g, &first, c7::PlaceOutcome::Created);
    let renamed = place(
        "Courtyard Kitchen",
        &[
            obs("osm_id", "node/9100001"),
            obs("location", FAR),
            obs("cuisine", "noodles"),
        ],
    );
    assert_eq!(placed(&mut g, &renamed, c7::PlaceOutcome::Found), id);
    assert_eq!(count(&g, "SELECT count(*) FROM bed_entity"), 1);
    assert_eq!(value(&g, &id, "cuisine"), "noodles");
    assert_eq!(pairs(&g), []);
    repeat_writes_nothing(&mut g, &renamed, &id);
}

#[test]
fn a_name_one_edit_away_within_forty_metres_merges_and_leaves_the_verdicts_alone() {
    let mut g = garden();
    let id = placed(
        &mut g,
        &at("Lantern Noodle Bar", BASE),
        c7::PlaceOutcome::Created,
    );
    let req: c7::PlacesVerdictRequest = serde_json::from_value(json!({
        "place": id, "verdict": 1, "note": "broth", "said_at": NOW - 86_400,
        "source_ref": "chat:msg-9101",
    }))
    .unwrap();
    g.garden_places_verdict(&req, NOW).unwrap();
    let verdicts = || "SELECT group_concat(seq || verdict || note || source_ref) FROM bed_verdict";
    let said: String = g
        .store()
        .conn()
        .query_row(verdicts(), [], |r| r.get(0))
        .unwrap();

    let near = place(
        "Lantern Noodle Bars",
        &[obs("location", NEAR_BASE), obs("cuisine", "noodles")],
    );
    let merged = write(&mut g, &near);
    assert_eq!(
        (merged.outcome, merged.id.as_deref()),
        (c7::PlaceOutcome::Found, Some(id.as_str()))
    );
    let outcomes: Vec<_> = merged.attrs.iter().map(|a| a.outcome).collect();
    assert_eq!(
        outcomes,
        [c7::AttrOutcome::Replaced, c7::AttrOutcome::Written],
        "the write's attributes are observations of the found place"
    );
    assert_eq!(value(&g, &id, "location"), NEAR_BASE);
    assert_eq!(count(&g, "SELECT count(*) FROM bed_attr_history"), 1);
    assert_eq!(count(&g, "SELECT count(*) FROM bed_entity"), 1);
    let still: String = g
        .store()
        .conn()
        .query_row(verdicts(), [], |r| r.get(0))
        .unwrap();
    assert_eq!(still, said);
    assert_eq!(pairs(&g), []);
    repeat_writes_nothing(&mut g, &near, &id);
}

#[test]
fn another_name_within_forty_metres_is_a_new_place_answered_as_a_possible_duplicate() {
    let mut g = garden();
    let lantern = placed(
        &mut g,
        &at("Lantern Noodle Bar", BASE),
        c7::PlaceOutcome::Created,
    );
    let harbour = at("Harbour Grill", NEAR_BASE);
    let grill = placed(&mut g, &harbour, c7::PlaceOutcome::Created);
    let (a, b) = ordered(&lantern, &grill);
    assert_eq!(pairs(&g), [(a, b, Some(40), None)]);

    let req: c7::PlacesSearchRequest =
        serde_json::from_value(json!({"near": {"lat": 0.5, "lon": -25.0}})).unwrap();
    let hits = g.garden_places_search(&req, NOW).unwrap().places;
    let dup: Vec<(&str, Option<&str>)> = hits
        .iter()
        .flat_map(|h| {
            h.marks
                .iter()
                .filter(|m| m.kind == c7::PlaceMarkKind::PossibleDuplicate)
                .map(|m| (h.id.as_str(), m.other.as_deref()))
        })
        .collect();
    assert_eq!(
        dup,
        [
            (lantern.as_str(), Some(grill.as_str())),
            (grill.as_str(), Some(lantern.as_str()))
        ]
    );
    repeat_writes_nothing(&mut g, &harbour, &grill);
}

#[test]
fn short_names_one_edit_apart_within_forty_metres_are_a_pair_not_a_merge() {
    let mut g = garden();
    let first = placed(&mut g, &at("Kumo", BASE), c7::PlaceOutcome::Created);
    let second = placed(&mut g, &at("Kuma", NEAR_BASE), c7::PlaceOutcome::Created);
    let (a, b) = ordered(&first, &second);
    assert_eq!(pairs(&g), [(a, b, Some(40), None)]);
}

#[test]
fn two_branches_of_one_name_two_km_apart_are_two_places_and_a_write_near_each_finds_its_own() {
    let mut g = garden();
    let south = placed(&mut g, &at("Copper Wok", BASE), c7::PlaceOutcome::Created);
    let north = placed(&mut g, &at("Copper Wok", FAR), c7::PlaceOutcome::Created);
    assert_ne!(south, north);
    assert_eq!(pairs(&g), []);

    let near_south = place(
        "Copper Wok",
        &[obs("location", NEAR_BASE), obs("cuisine", "wok")],
    );
    assert_eq!(placed(&mut g, &near_south, c7::PlaceOutcome::Found), south);
    let near_north = place(
        "copper  wok",
        &[obs("location", NEAR_FAR), obs("cuisine", "noodles")],
    );
    assert_eq!(placed(&mut g, &near_north, c7::PlaceOutcome::Found), north);
    assert_eq!(
        (value(&g, &south, "cuisine"), value(&g, &north, "cuisine")),
        ("wok".into(), "noodles".into())
    );
    assert_eq!(count(&g, "SELECT count(*) FROM bed_entity"), 2);

    let by_name = write(&mut g, &place("Copper Wok", &[obs("rating", "4.0")]));
    assert_eq!(
        (by_name.outcome, by_name.refused),
        (
            c7::PlaceOutcome::Refused,
            Some(c7::PlaceRefusal::AmbiguousPlace)
        ),
        "a write by name only cannot tell the branches apart"
    );
    assert_eq!(pairs(&g), []);
    repeat_writes_nothing(&mut g, &near_north, &north);
}

#[test]
fn a_write_by_name_only_finds_the_one_namesake_wherever_it_is() {
    let mut g = garden();
    let id = placed(&mut g, &at("Brass Bowl", BASE), c7::PlaceOutcome::Created);
    let by_name = place("brass bowl", &[obs("cuisine", "ramen")]);
    assert_eq!(placed(&mut g, &by_name, c7::PlaceOutcome::Found), id);
    assert_eq!(
        placed(&mut g, &place("Brass Bowl", &[]), c7::PlaceOutcome::Found),
        id
    );
    assert_eq!(pairs(&g), []);
    repeat_writes_nothing(&mut g, &by_name, &id);
}

#[test]
fn a_similar_name_where_either_side_has_no_location_is_a_new_place_with_a_pair() {
    let mut g = garden();
    let located = placed(
        &mut g,
        &at("Tidepool Kitchen", BASE),
        c7::PlaceOutcome::Created,
    );
    let unlocated = place("Tidepool Kitchn", &[obs("cuisine", "seafood")]);
    let other = placed(&mut g, &unlocated, c7::PlaceOutcome::Created);
    let (a, b) = ordered(&located, &other);
    assert_eq!(pairs(&g), [(a, b, None, None)]);
    repeat_writes_nothing(&mut g, &unlocated, &other);

    let mut g = garden();
    let bare = placed(
        &mut g,
        &place("Saltmarsh Grill", &[obs("cuisine", "grill")]),
        c7::PlaceOutcome::Created,
    );
    let with_point = at("Saltmarsh Grill", BASE);
    let branch = placed(&mut g, &with_point, c7::PlaceOutcome::Created);
    let (a, b) = ordered(&bare, &branch);
    assert_eq!(pairs(&g), [(a, b, None, None)]);
    repeat_writes_nothing(&mut g, &with_point, &branch);
}

#[test]
fn a_transliterated_name_without_a_location_is_a_new_place_with_a_pair() {
    let mut g = garden();
    let latin = placed(
        &mut g,
        &at("Pelmennaja Mira", BASE),
        c7::PlaceOutcome::Created,
    );
    let cyrillic = place("Пельменная Мира", &[obs("cuisine", "dumplings")]);
    let other = placed(&mut g, &cyrillic, c7::PlaceOutcome::Created);
    let (a, b) = ordered(&latin, &other);
    assert_eq!(pairs(&g), [(a, b, None, None)]);
    repeat_writes_nothing(&mut g, &cyrillic, &other);
}
