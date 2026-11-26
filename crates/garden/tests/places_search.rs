//! `places_search` through the C7 facade on an in-memory garden, filled through the C7 writes
//! or the places file importer. Every place, point, source and verdict is invented.

#![allow(missing_docs, clippy::unwrap_used, clippy::needless_pass_by_value)]

use serde_json::{Value, json};
use ulei_garden::api::{ApiError, Garden, Hybrid};
use ulei_garden::miss::ProducerKey;
use ulei_garden::places_import::{self, BedWire, CallError};
use ulei_garden::rerank::NoReranker;
use ulei_garden::store::Store;
use ulei_garden::wire::c7;

const DAY: i64 = 86_400;
/// 2031-03-20 00:00 UTC, the `as_of` of the search fixture.
const NOW: i64 = 1_931_731_200;
const FIXTURE: &str = include_str!("fixtures/places-search.md");

fn garden() -> Garden {
    Garden::new(
        Store::open_in_memory().unwrap(),
        Hybrid::new(None, Box::new(NoReranker)),
        ulei_garden::api::ApiConfig::default(),
        ProducerKey::from_bytes(&[0x5a; 32]).unwrap(),
    )
}

/// The C7 writes of the importer answered by an in-process garden at [`NOW`].
struct Wire<'a>(&'a mut Garden);

impl BedWire for Wire<'_> {
    fn places_write(
        &mut self,
        req: &c7::PlacesWriteRequest,
    ) -> Result<c7::PlacesWriteResponse, CallError> {
        self.0
            .garden_places_write(req, NOW)
            .map_err(|e| CallError::Failed(e.to_string()))
    }

    fn places_verdict(
        &mut self,
        req: &c7::PlacesVerdictRequest,
    ) -> Result<c7::PlacesVerdictResponse, CallError> {
        self.0
            .garden_places_verdict(req, NOW)
            .map_err(|e| CallError::Failed(e.to_string()))
    }

    fn places_profile(
        &mut self,
        req: &c7::PlacesProfileRequest,
    ) -> Result<c7::PlacesProfileResponse, CallError> {
        self.0
            .garden_places_profile(req)
            .map_err(|e| CallError::Failed(e.to_string()))
    }
}

fn imported(text: &str) -> Garden {
    let mut g = garden();
    let plan = places_import::check(text, "places-search.md", NOW).unwrap();
    let written = places_import::write(&plan, &mut Wire(&mut g)).unwrap();
    assert!(written.refused.is_empty(), "{:?}", written.to_json());
    g
}

fn search(g: &Garden, req: Value) -> Vec<c7::PlaceHit> {
    let req: c7::PlacesSearchRequest = serde_json::from_value(req).unwrap();
    g.garden_places_search(&req, NOW).unwrap().places
}

fn names(hits: &[c7::PlaceHit]) -> Vec<&str> {
    hits.iter().map(|h| h.name.as_str()).collect()
}

fn invalid(g: &Garden, req: Value) -> String {
    let req: c7::PlacesSearchRequest = serde_json::from_value(req).unwrap();
    match g.garden_places_search(&req, NOW) {
        Err(ApiError::Invalid(m)) => m,
        other => panic!("not invalid: {other:?}"),
    }
}

/// Writes places through `places_write` at [`NOW`].
fn write(g: &mut Garden, places: Value) {
    let req: c7::PlacesWriteRequest = serde_json::from_value(json!({ "places": places })).unwrap();
    let out = g.garden_places_write(&req, NOW).unwrap();
    for p in &out.places {
        assert!(p.id.is_some(), "{p:?}");
        assert!(
            p.attrs
                .iter()
                .all(|a| a.outcome != c7::AttrOutcome::Refused),
            "{p:?}"
        );
    }
}

fn obs(key: &str, value: &str, age_days: i64) -> Value {
    json!({"key": key, "value": value, "checked_at": NOW - age_days * DAY, "source": "fixture-maps"})
}

fn profile(g: &mut Garden, key: &str, value: &str) {
    let req: c7::PlacesProfileRequest = serde_json::from_value(
        json!({"key": key, "value": value, "source_ref": "owner", "set_at": NOW - DAY}),
    )
    .unwrap();
    g.garden_places_profile(&req).unwrap();
}

fn noodles_near() -> Value {
    json!({
        "filters": {"cuisine": ["noodles"], "max_distance_m": 2000, "min_rating": 4.6},
        "near": {"lat": 0.5, "lon": -25.0},
    })
}

#[test]
fn cuisine_distance_and_rating_find_three_places_ranked_verdict_then_rating_then_distance() {
    let g = imported(FIXTURE);
    let hits = search(&g, noodles_near());
    assert_eq!(
        names(&hits),
        ["Lantern Noodle Bar", "Brass Bowl", "Copper Wok"],
        "verdict +1 at 4.6 above 4.9 and 4.7; 4.9 at 1.5 km above 4.7 at 0.4 km; Ember \
         Noodles with its latest verdict -1, another cuisine, 2.5 km, 4.5, a banned cuisine \
         and a gone place left out"
    );
    let distances: Vec<_> = hits.iter().map(|h| h.distance_m).collect();
    assert_eq!(distances, [Some(1801), Some(1501), Some(400)]);
    let lantern = &hits[0];
    let verdict = lantern.verdict.as_ref().unwrap();
    assert_eq!(
        (
            verdict.verdict,
            verdict.note.as_deref(),
            verdict.source_ref.as_str()
        ),
        (1, Some("broth was excellent"), "chat:msg-7102")
    );
    assert_eq!(lantern.status, c7::PlaceStatus::Active);
    assert!(lantern.open.is_none());
    let copper = &hits[2];
    assert_eq!(
        copper.marks,
        [c7::PlaceMark {
            kind: c7::PlaceMarkKind::BanRisk,
            key: None,
            age_days: None,
            terms: Some(vec!["celery".into()]),
            other: None,
        }],
        "a ban its warnings name marks the place and does not drop it"
    );
    let keys: Vec<_> = copper.attrs.iter().map(|a| a.key.as_str()).collect();
    assert_eq!(
        keys,
        ["cuisine", "location", "rating", "review_count", "warnings"]
    );
    for a in &copper.attrs {
        assert_eq!(
            (a.age_days, a.stale, a.confidence),
            (10, false, None),
            "{a:?}"
        );
    }
}

#[test]
fn the_latest_verdict_of_minus_one_drops_a_place_and_a_later_plus_one_brings_it_back() {
    let mut g = imported(FIXTURE);
    let req = noodles_near();
    assert!(!names(&search(&g, req.clone())).contains(&"Ember Noodles"));
    let ember = id(&g, "Ember Noodles");
    let v: c7::PlacesVerdictRequest = serde_json::from_value(json!({
        "place": ember, "verdict": 1, "said_at": NOW - DAY, "source_ref": "chat:msg-7105"
    }))
    .unwrap();
    g.garden_places_verdict(&v, NOW).unwrap();
    assert_eq!(
        names(&search(&g, req)),
        [
            "Ember Noodles",
            "Lantern Noodle Bar",
            "Brass Bowl",
            "Copper Wok"
        ],
        "two liked places order by quality"
    );
}

/// The id of the place entity `name`.
fn id(g: &Garden, name: &str) -> String {
    g.store()
        .conn()
        .query_row(
            "SELECT id FROM entities WHERE kind = 'place' AND name = ?1",
            [name],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn a_stale_rating_demotes_and_marks_a_place_and_never_drops_it() {
    let mut g = garden();
    let place = |name: &str, rating_age: i64| {
        json!({"name": name, "status": "active", "source": "fixture", "attrs": [
            obs("location", "0.5000, -25.0000", 1),
            obs("cuisine", "noodles", 1),
            obs("rating", "4.5", rating_age),
            obs("review_count", "100", 1),
        ]})
    };
    write(
        &mut g,
        json!([place("Alder Noodles", 40), place("Birch Noodles", 3)]),
    );
    let hits = search(
        &g,
        json!({"filters": {"cuisine": ["noodles"], "min_rating": 4.5}}),
    );
    assert_eq!(
        names(&hits),
        ["Birch Noodles", "Alder Noodles"],
        "the fresh one first though the stale one's id sorts first"
    );
    assert_eq!(hits[0].marks, []);
    assert_eq!(
        hits[1].marks,
        [c7::PlaceMark {
            kind: c7::PlaceMarkKind::Stale,
            key: Some("rating".into()),
            age_days: Some(40),
            terms: None,
            other: None,
        }]
    );
    let rating = hits[1].attrs.iter().find(|a| a.key == "rating").unwrap();
    assert_eq!((rating.age_days, rating.stale), (40, true));
}

#[test]
fn a_profile_threshold_keeps_a_place_without_the_attribute_and_a_requested_one_drops_it() {
    let mut g = garden();
    profile(&mut g, "min_rating", "4.2");
    profile(&mut g, "budget_for_two", "40-60");
    let place = |name: &str, attrs: Value| json!({"name": name, "status": "candidate", "source": "fixture", "attrs": attrs});
    write(
        &mut g,
        json!([
            place("Unrated Pho", json!([obs("cuisine", "pho", 1)])),
            place(
                "Low Pho",
                json!([obs("cuisine", "pho", 1), obs("rating", "4.0", 1)])
            ),
            place(
                "Good Pho",
                json!([
                    obs("cuisine", "pho", 1),
                    obs("rating", "4.4", 1),
                    obs("price_for_two", "50-70", 1)
                ])
            ),
            place(
                "Dear Pho",
                json!([
                    obs("cuisine", "pho", 1),
                    obs("rating", "4.4", 1),
                    obs("price_for_two", "61-90", 1)
                ])
            ),
        ]),
    );
    assert_eq!(
        names(&search(&g, json!({}))),
        ["Good Pho", "Unrated Pho"],
        "4.0 under the profile's 4.2 and a price from 61 over its budget up to 60 left out"
    );
    assert_eq!(
        names(&search(&g, json!({"filters": {"min_rating": 4.2}}))),
        ["Good Pho"]
    );
    assert_eq!(
        names(&search(&g, json!({"filters": {"max_price_for_two": 90}}))),
        ["Dear Pho", "Good Pho"],
        "a requested budget drops the places without a price"
    );
}

#[test]
fn bans_in_cuisine_drop_and_terms_match_words_after_normalisation() {
    let mut g = garden();
    profile(&mut g, "ban", "Sea Urchin");
    let place = |name: &str, cuisine: &str| {
        json!({"name": name, "status": "active", "source": "fixture",
               "attrs": [obs("cuisine", cuisine, 1)]})
    };
    write(
        &mut g,
        json!([
            place("Urchin Bar", "sushi, sea  urchin rolls"),
            place("Hand Roll", "Sushi"),
            place("Urchinless", "seaurchin"),
        ]),
    );
    assert_eq!(
        names(&search(&g, json!({"filters": {"cuisine": ["SUSHI"]}}))),
        ["Hand Roll"]
    );
    assert_eq!(names(&search(&g, json!({}))), ["Hand Roll", "Urchinless"]);
}

#[test]
fn maps_url_is_the_attribute_else_the_location_else_the_name_and_address() {
    let mut g = garden();
    write(
        &mut g,
        json!([
            {"name": "Linked", "status": "active", "source": "fixture", "attrs": [
                obs("maps_url", "https://maps.example.org/p/1", 1),
                obs("location", "0.5041, -25.0032", 1)]},
            {"name": "Located", "status": "active", "source": "fixture", "attrs": [
                obs("location", "0.5041, -25.0032", 1)]},
            {"name": "Tidepool Kitchen", "status": "candidate", "source": "fixture", "attrs": [
                obs("address", "Harbour Row 3", 1)]},
            {"name": "Чайхана", "status": "candidate", "source": "fixture"},
        ]),
    );
    let hits = search(&g, json!({}));
    let urls: Vec<(&str, &str)> = hits
        .iter()
        .map(|h| (h.name.as_str(), h.maps_url.as_str()))
        .collect();
    let base = "https://www.google.com/maps/search/?api=1&query=";
    assert_eq!(
        urls,
        [
            ("Linked", "https://maps.example.org/p/1".to_owned()),
            ("Located", format!("{base}0.5041%2C-25.0032")),
            (
                "Tidepool Kitchen",
                format!("{base}Tidepool%20Kitchen%20Harbour%20Row%203")
            ),
            (
                "Чайхана",
                format!("{base}%D0%A7%D0%B0%D0%B9%D1%85%D0%B0%D0%BD%D0%B0")
            ),
        ]
        .iter()
        .map(|(n, u)| (*n, u.as_str()))
        .collect::<Vec<_>>()
    );
    assert!(
        hits.iter().all(|h| h.distance_m.is_none()),
        "no point, no distance"
    );
}

#[test]
fn vibe_stems_rank_above_quality_and_meet_other_word_forms() {
    let mut g = garden();
    let place = |name: &str, rating: &str, vibe: &str| {
        json!({"name": name, "status": "active", "source": "fixture", "attrs": [
            obs("rating", rating, 1), obs("vibe", vibe, 1)]})
    };
    write(
        &mut g,
        json!([
            place("Bright Hall", "4.9", "loud, bright"),
            place("Courtyard", "4.3", "уютный дворик"),
        ]),
    );
    assert_eq!(names(&search(&g, json!({}))), ["Bright Hall", "Courtyard"]);
    assert_eq!(
        names(&search(&g, json!({"vibe": "уютные"}))),
        ["Courtyard", "Bright Hall"]
    );
}

#[test]
fn status_limit_open_now_and_unsettled_duplicates_shape_the_answer() {
    let g = imported(FIXTURE);
    assert_eq!(
        names(&search(&g, json!({"filters": {"status": ["gone"]}}))),
        ["Old Pier Noodles"]
    );
    let mut req = noodles_near();
    req["limit"] = json!(2);
    assert_eq!(
        names(&search(&g, req)),
        ["Lantern Noodle Bar", "Brass Bowl"]
    );

    let mut req = noodles_near();
    req["open_now"] = json!(true);
    req["local_time"] = json!("2031-03-20T19:30");
    let hits = search(&g, req);
    assert_eq!(
        names(&hits),
        ["Lantern Noodle Bar", "Brass Bowl", "Copper Wok"]
    );
    assert!(
        hits.iter().all(|h| h.open == Some(c7::OpenState::Unknown)),
        "no hours read: open is unknown"
    );

    let (brass, copper, lantern) = (
        id(&g, "Brass Bowl"),
        id(&g, "Copper Wok"),
        id(&g, "Lantern Noodle Bar"),
    );
    let conn = g.store().conn();
    for (a, b, settled) in [(&brass, &copper, None), (&brass, &lantern, Some(NOW))] {
        let (a, b) = if a < b { (a, b) } else { (b, a) };
        conn.execute(
            "INSERT INTO bed_dup_pair (a, b, distance_m, found_at, settled_at)
             VALUES (?1, ?2, 50, ?3, ?4)",
            rusqlite::params![a, b, NOW, settled],
        )
        .unwrap();
    }
    let hits = search(&g, noodles_near());
    let dup = |h: &c7::PlaceHit| -> Vec<String> {
        h.marks
            .iter()
            .filter(|m| m.kind == c7::PlaceMarkKind::PossibleDuplicate)
            .map(|m| m.other.clone().unwrap())
            .collect()
    };
    assert_eq!(
        hits.iter().map(dup).collect::<Vec<_>>(),
        [vec![], vec![copper.clone()], vec![brass.clone()]]
    );
}

#[test]
fn requests_outside_the_c7_rules_are_invalid() {
    let g = garden();
    assert!(invalid(&g, json!({"filters": {"max_distance_m": 2000}})).contains("max_distance_m"));
    assert!(invalid(&g, json!({"open_now": true})).contains("local_time"));
    assert!(invalid(&g, json!({"limit": 0})).contains("limit"));
    let long = "n".repeat(101);
    let eleven: Vec<String> = (0..11).map(|n| format!("cuisine{n}")).collect();
    for (filters, rule) in [
        (json!({"status": []}), "`status` holds 1 to 3"),
        (
            json!({"status": ["active", "candidate", "gone", "active"]}),
            "`status` holds 1 to 3",
        ),
        (json!({"status": ["gone", "gone"]}), "`status` repeats"),
        (json!({"min_rating": -0.1}), "`min_rating` is 0 to 5"),
        (json!({"min_rating": 5.1}), "`min_rating` is 0 to 5"),
        (
            json!({"max_distance_m": 0}),
            "`max_distance_m` is 1 to 100000",
        ),
        (
            json!({"max_distance_m": 100_001}),
            "`max_distance_m` is 1 to 100000",
        ),
        (
            json!({"max_price_for_two": -1}),
            "`max_price_for_two` is at least 0",
        ),
        (json!({"cuisine": []}), "`cuisine` holds 1 to 10"),
        (json!({"cuisine": eleven}), "`cuisine` holds 1 to 10"),
        (json!({"cuisine": [""]}), "`cuisine` term holds 1 to 100"),
        (json!({"cuisine": [long]}), "`cuisine` term holds 1 to 100"),
    ] {
        let req = json!({"filters": filters, "near": {"lat": 0.5, "lon": -25.0}});
        let message = invalid(&g, req.clone());
        assert!(message.contains(rule), "{req}: {message}");
    }
    let edges = json!({"filters": {
        "status": ["active", "candidate", "gone"], "min_rating": 5, "max_distance_m": 100_000,
        "max_price_for_two": 0, "cuisine": ["n".repeat(100)],
    }, "near": {"lat": 0.5, "lon": -25.0}});
    assert_eq!(
        names(&search(&g, edges)),
        [] as [&str; 0],
        "every bound itself is allowed"
    );
    let mut g = garden();
    profile(&mut g, "home", "0.5000, -25.0000");
    assert_eq!(
        names(&search(&g, json!({"filters": {"max_distance_m": 2000}}))),
        [] as [&str; 0],
        "the profile's home is the point"
    );
}

#[test]
fn the_profile_home_is_the_origin_of_distances_and_of_the_box_without_near() {
    let mut g = garden();
    profile(&mut g, "home", "0.5000, -25.0000");
    let place = |name: &str, lat: &str| {
        json!({"name": name, "status": "active", "source": "fixture",
               "attrs": [obs("location", &format!("{lat}, -25.0000"), 1)]})
    };
    write(
        &mut g,
        json!([
            place("Close", "0.5036"),
            place("Middle", "0.5135"),
            place("Beyond", "0.5225"),
        ]),
    );
    let distances = |req: Value| -> Vec<(String, Option<u64>)> {
        search(&g, req)
            .into_iter()
            .map(|h| (h.name, h.distance_m))
            .collect()
    };
    let at = |pairs: &[(&str, u64)]| -> Vec<(String, Option<u64>)> {
        pairs
            .iter()
            .map(|(n, d)| ((*n).to_owned(), Some(*d)))
            .collect()
    };
    assert_eq!(
        distances(json!({})),
        at(&[("Close", 400), ("Middle", 1501), ("Beyond", 2502)])
    );
    assert_eq!(
        distances(json!({"filters": {"max_distance_m": 2000}})),
        at(&[("Close", 400), ("Middle", 1501)]),
        "a place 2.5 km from home is past 2000 m"
    );
    assert_eq!(
        distances(
            json!({"near": {"lat": 0.5225, "lon": -25.0}, "filters": {"max_distance_m": 2000}})
        ),
        at(&[("Beyond", 0), ("Middle", 1001)]),
        "`near` replaces home as the origin"
    );
}
