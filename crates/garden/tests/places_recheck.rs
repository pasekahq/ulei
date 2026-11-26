//! The worker's recheck pass of the places bed through `ulei_garden::worker` on an injected
//! clock, with an in-process fake `places::Recheck` source, over a bed imported from a synthetic
//! fixture. Every place, `osm_id`, value, source and date is invented.

#![allow(missing_docs, clippy::unwrap_used)]

use std::cell::Cell;
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use ulei_garden::api::{ApiConfig, Garden, Hybrid};
use ulei_garden::edge::EdgeConfig;
use ulei_garden::miss::{Inbox, ProducerKey};
use ulei_garden::places::{
    Answer, Asked, Closing, RECHECK_FIRST_RETRY, RECHECK_MAX_RETRY, Recheck, Seen,
};
use ulei_garden::places_import::{self, BedWire, CallError};
use ulei_garden::places_search;
use ulei_garden::rerank::NoReranker;
use ulei_garden::second::SourceError;
use ulei_garden::store::Store;
use ulei_garden::wire::c7;
use ulei_garden::worker::{Clock, Schedule, Worker};

const DAY: i64 = 86_400;
const HOUR: i64 = 3_600;
/// 2031-03-20 00:00 UTC, the `as_of` of the fixture.
const NOW: i64 = 1_931_731_200;
const FIXTURE: &str = include_str!("fixtures/places-recheck.md");
const SCHEDULE: Schedule = Schedule {
    expire_interval: 60,
    recheck_interval: HOUR,
    recheck_batch: 8,
    maintain_interval: 1_000 * DAY,
};
const CHILD: &str = "ULEI_GARDEN_RECHECK_CHILD";

/// A clock the test sets.
#[derive(Clone, Default)]
struct Fake(Rc<Cell<i64>>);

impl Clock for Fake {
    fn now(&self) -> i64 {
        self.0.get()
    }
}

type Script = dyn Fn(&[Asked], i64) -> Result<Vec<Answer>, SourceError> + Send;

/// A recheck source that answers by its script and records every call.
struct Source {
    calls: Arc<Mutex<Vec<Vec<Asked>>>>,
    script: Box<Script>,
}

impl Recheck for Source {
    fn recheck(&self, places: &[Asked], now: i64) -> Result<Vec<Answer>, SourceError> {
        self.calls.lock().unwrap().push(places.to_vec());
        (self.script)(places, now)
    }
}

/// A fresh directory holding a store, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("ulei-garden-recheck-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        Self(dir)
    }

    fn store(&self) -> PathBuf {
        self.0.join("garden.sqlite3")
    }

    fn key() -> ProducerKey {
        ProducerKey::from_bytes(&[0x5a; 32]).unwrap()
    }

    /// The fixture imported through the C7 writes into the store.
    fn imported(&self) {
        let mut g = Garden::new(
            Store::open(&self.store()).unwrap(),
            Hybrid::new(None, Box::new(NoReranker)),
            ApiConfig::default(),
            Self::key(),
        );
        let plan = places_import::check(FIXTURE, "places-recheck.md", NOW).unwrap();
        let written = places_import::write(&plan, &mut Wire(&mut g)).unwrap();
        assert!(written.refused.is_empty(), "{:?}", written.to_json());
    }

    /// A worker over the imported fixture at `clock`, with `source` when given.
    fn worker(&self, clock: &Fake, source: Option<Source>) -> Worker<Fake> {
        self.imported();
        let worker = Worker::new(
            Store::open(&self.store()).unwrap(),
            SCHEDULE,
            clock.clone(),
            Inbox::new(self.0.join("inbox.sock"), Self::key()),
            None,
            EdgeConfig::default(),
        )
        .unwrap();
        match source {
            Some(s) => worker.with_recheck(Box::new(s)),
            None => worker,
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
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

fn source(
    script: impl Fn(&[Asked], i64) -> Result<Vec<Answer>, SourceError> + Send + 'static,
) -> (Source, Arc<Mutex<Vec<Vec<Asked>>>>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let s = Source {
        calls: Arc::clone(&calls),
        script: Box::new(script),
    };
    (s, calls)
}

fn seen(key: &str, value: &str, checked_at: i64) -> Seen {
    Seen {
        key: key.into(),
        value: value.into(),
        checked_at,
        source: "fixture-recheck".into(),
        confidence: 1.0,
    }
}

fn answer(place: &Asked, seen: Vec<Seen>, closed: Option<Closing>) -> Answer {
    Answer {
        place: place.place.clone(),
        seen,
        closed,
    }
}

fn by_osm<'a>(places: &'a [Asked], osm_id: &str) -> Option<&'a Asked> {
    places.iter().find(|a| a.osm_id == osm_id)
}

/// The entity id of the place named `name`.
fn id(path: &Path, name: &str) -> String {
    Store::open(path)
        .unwrap()
        .conn()
        .query_row("SELECT id FROM entities WHERE name = ?1", [name], |r| {
            r.get(0)
        })
        .unwrap()
}

/// `tries` and `next_attempt` of the `bed_recheck` row of `name` and `key`.
fn queued(path: &Path, name: &str, key: &str) -> Option<(i64, i64)> {
    Store::open(path)
        .unwrap()
        .conn()
        .query_row(
            "SELECT r.tries, r.next_attempt FROM bed_recheck r JOIN entities e ON e.id = r.entity
             WHERE e.name = ?1 AND r.key = ?2",
            [name, key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .unwrap()
}

/// `value`, `checked_at` and `source` of the current `bed_attr` row of `name` and `key`.
fn attr(path: &Path, name: &str, key: &str) -> (String, i64, String) {
    Store::open(path)
        .unwrap()
        .conn()
        .query_row(
            "SELECT a.value, a.checked_at, a.source FROM bed_attr a
             JOIN entities e ON e.id = a.entity WHERE e.name = ?1 AND a.key = ?2",
            [name, key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
}

/// `value`, `checked_at` and `replaced_at` of the history rows of `name` and `key`.
fn history(path: &Path, name: &str, key: &str) -> Vec<(String, i64, i64)> {
    let store = Store::open(path).unwrap();
    let mut stmt = store
        .conn()
        .prepare(
            "SELECT h.value, h.checked_at, h.replaced_at FROM bed_attr_history h
             JOIN entities e ON e.id = h.entity WHERE e.name = ?1 AND h.key = ?2 ORDER BY h.seq",
        )
        .unwrap();
    stmt.query_map([name, key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// Every row of `bed_verdict` and `bed_profile` as text.
fn verdicts_and_profile(path: &Path) -> Vec<String> {
    let store = Store::open(path).unwrap();
    let mut rows = Vec::new();
    for sql in [
        "SELECT seq, entity, verdict, note, said_at, source_ref, better_than, written_at
         FROM bed_verdict ORDER BY seq",
        "SELECT type, key, value, source_ref, set_at FROM bed_profile ORDER BY type, key, value",
    ] {
        let mut stmt = store.conn().prepare(sql).unwrap();
        let n = stmt.column_count();
        let found: Vec<String> = stmt
            .query_map([], |r| {
                let cols: Vec<String> = (0..n)
                    .map(|i| format!("{:?}", r.get_ref(i).unwrap()))
                    .collect();
                Ok(cols.join("|"))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        rows.extend(found);
    }
    rows
}

fn search(path: &Path, req: Value, now: i64) -> Vec<c7::PlaceHit> {
    let req: c7::PlacesSearchRequest = serde_json::from_value(req).unwrap();
    places_search::search(Store::open(path).unwrap().conn(), &req, now)
        .unwrap()
        .places
}

fn hit<'a>(hits: &'a [c7::PlaceHit], name: &str) -> &'a c7::PlaceHit {
    hits.iter().find(|h| h.name == name).unwrap()
}

fn hit_attr<'a>(h: &'a c7::PlaceHit, key: &str) -> &'a c7::PlaceAttr {
    h.attrs.iter().find(|a| a.key == key).unwrap()
}

fn stale_marks(h: &c7::PlaceHit) -> Vec<&str> {
    h.marks
        .iter()
        .filter(|m| m.kind == c7::PlaceMarkKind::Stale)
        .filter_map(|m| m.key.as_deref())
        .collect()
}

#[test]
fn stale_hours_are_queued_rechecked_by_osm_id_and_current_after_the_pass() {
    let scratch = Scratch::new();
    let path = scratch.store();
    let clock = Fake::default();
    clock.0.set(NOW);
    let (src, calls) = source(|places, _| {
        let juniper = by_osm(places, "node/9100001").unwrap();
        Ok(vec![answer(
            juniper,
            vec![seen("hours", "Mo-Su 11:00-23:00", NOW - HOUR)],
            None,
        )])
    });
    let mut worker = scratch.worker(&clock, Some(src));
    let before = verdicts_and_profile(&path);
    let found = search(&path, json!({}), NOW);
    assert!(hit_attr(hit(&found, "Juniper Ramen"), "hours").stale);
    assert_eq!(stale_marks(hit(&found, "Juniper Ramen")), ["rating"]);

    assert_eq!(
        worker.tick(),
        NOW + HOUR,
        "the next pass, for the keys left queued"
    );

    let calls = calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    let mut handed: Vec<(&str, Vec<String>)> = calls[0]
        .iter()
        .map(|a| (a.osm_id.as_str(), a.keys.clone()))
        .collect();
    handed.sort();
    assert_eq!(
        handed,
        [
            (
                "node/9100001",
                vec!["hours".to_owned(), "rating".to_owned()]
            ),
            (
                "node/9100003",
                vec!["hours".to_owned(), "operating_status".to_owned()]
            ),
            ("way/9100004", vec!["hours".to_owned()]),
        ]
    );
    assert_eq!(
        by_osm(&calls[0], "node/9100001").unwrap().place.0,
        id(&path, "Juniper Ramen")
    );

    assert_eq!(
        attr(&path, "Juniper Ramen", "hours"),
        (
            "Mo-Su 11:00-23:00".to_owned(),
            NOW - HOUR,
            "fixture-recheck".to_owned()
        )
    );
    assert_eq!(
        history(&path, "Juniper Ramen", "hours"),
        [("Mo-Su 11:00-22:00".to_owned(), NOW - 10 * DAY, NOW)]
    );
    assert_eq!(queued(&path, "Juniper Ramen", "hours"), None);
    assert_eq!(
        queued(&path, "Juniper Ramen", "rating"),
        Some((1, NOW + RECHECK_FIRST_RETRY)),
        "a queued key without an answer backs off"
    );
    assert_eq!(queued(&path, "Moss Cellar", "hours"), Some((0, NOW)));

    let found = search(&path, json!({}), NOW);
    let juniper = hit(&found, "Juniper Ramen");
    let hours = hit_attr(juniper, "hours");
    assert!(!hours.stale);
    assert_eq!(hours.source, "fixture-recheck");
    assert_eq!(stale_marks(juniper), ["rating"]);
    assert_eq!(verdicts_and_profile(&path), before);
}

#[test]
fn an_equal_value_moves_only_checked_at_and_clears_its_row() {
    let scratch = Scratch::new();
    let path = scratch.store();
    let clock = Fake::default();
    clock.0.set(NOW);
    let (src, _) = source(|places, _| {
        let fern = by_osm(places, "way/9100004").unwrap();
        Ok(vec![answer(
            fern,
            vec![seen("hours", "Mo-Fr 08:00-16:00", NOW - 2 * HOUR)],
            None,
        )])
    });
    let mut worker = scratch.worker(&clock, Some(src));
    let before = verdicts_and_profile(&path);
    worker.tick();

    assert_eq!(
        attr(&path, "Fern Diner", "hours"),
        (
            "Mo-Fr 08:00-16:00".to_owned(),
            NOW - 2 * HOUR,
            "fixture-recheck".to_owned()
        )
    );
    assert_eq!(
        history(&path, "Fern Diner", "hours"),
        [("Mo-Fr 08:00-16:00".to_owned(), NOW - 9 * DAY, NOW)]
    );
    assert_eq!(queued(&path, "Fern Diner", "hours"), None);
    assert!(!hit_attr(hit(&search(&path, json!({}), NOW), "Fern Diner"), "hours").stale);
    assert_eq!(verdicts_and_profile(&path), before);
}

#[test]
fn a_failing_source_backs_off_from_one_hour_doubling_up_to_seven_days() {
    let scratch = Scratch::new();
    let path = scratch.store();
    let clock = Fake::default();
    clock.0.set(NOW);
    let (src, calls) = source(|_, _| Err(SourceError::new("unreachable")));
    let mut worker = scratch.worker(&clock, Some(src));
    let before = verdicts_and_profile(&path);
    let mut waits = Vec::new();
    for tries in 1..=10 {
        let now = clock.now();
        let deadline = worker.tick();
        let (t, next) = queued(&path, "Juniper Ramen", "hours").unwrap();
        assert_eq!(t, tries);
        assert_eq!(deadline, next, "the earliest next_attempt is the deadline");
        assert_eq!(
            queued(&path, "Lark Bakery", "operating_status").unwrap().0,
            tries
        );
        waits.push(next - now);
        clock.0.set(deadline);
    }
    let h = HOUR;
    assert_eq!(
        waits,
        [
            h,
            2 * h,
            4 * h,
            8 * h,
            16 * h,
            32 * h,
            64 * h,
            128 * h,
            RECHECK_MAX_RETRY,
            RECHECK_MAX_RETRY
        ]
    );
    assert_eq!(calls.lock().unwrap().len(), 10);
    assert_eq!(attr(&path, "Juniper Ramen", "hours").1, NOW - 10 * DAY);
    assert_eq!(queued(&path, "Moss Cellar", "hours"), Some((0, NOW)));
    assert_eq!(verdicts_and_profile(&path), before);
}

/// Two passes on a failing source in a child process of this test binary, whose stderr the
/// parent reads.
#[test]
fn a_failed_call_is_one_stderr_line_with_its_class() {
    if std::env::var_os(CHILD).is_some() {
        let scratch = Scratch::new();
        let clock = Fake::default();
        clock.0.set(NOW);
        let (src, _) = source(|_, _| Err(SourceError::new("unreachable")));
        let mut worker = scratch.worker(&clock, Some(src));
        let deadline = worker.tick();
        clock.0.set(deadline);
        worker.tick();
        return;
    }
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "a_failed_call_is_one_stderr_line_with_its_class",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stderr = String::from_utf8(out.stderr).unwrap();
    let lines: Vec<&str> = stderr.lines().collect();
    assert_eq!(
        lines,
        [
            "garden: places recheck: unreachable",
            "garden: places recheck: unreachable"
        ]
    );
}

#[test]
fn a_closing_date_before_now_sets_the_place_gone_and_keeps_its_attributes_and_verdicts() {
    let scratch = Scratch::new();
    let path = scratch.store();
    let clock = Fake::default();
    clock.0.set(NOW);
    let closing = |at| Closing {
        at,
        checked_at: NOW - HOUR,
        source: "fixture-recheck".into(),
    };
    let (src, _) = source(move |places, _| {
        let lark = by_osm(places, "node/9100003");
        let fern = by_osm(places, "way/9100004");
        Ok(lark
            .map(|p| answer(p, Vec::new(), Some(closing(NOW - DAY))))
            .into_iter()
            .chain(fern.map(|p| answer(p, Vec::new(), Some(closing(NOW + 30 * DAY)))))
            .collect())
    });
    let mut worker = scratch.worker(&clock, Some(src));
    let before = verdicts_and_profile(&path);
    worker.tick();

    let (status, status_at): (String, i64) = Store::open(&path)
        .unwrap()
        .conn()
        .query_row(
            "SELECT status, status_at FROM bed_entity WHERE entity = ?1",
            [id(&path, "Lark Bakery")],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((status.as_str(), status_at), ("gone", NOW));
    assert_eq!(
        attr(&path, "Lark Bakery", "operating_status"),
        (
            "permanently_closed".to_owned(),
            NOW - HOUR,
            "fixture-recheck".to_owned()
        )
    );
    assert_eq!(attr(&path, "Lark Bakery", "hours").0, "Mo-Sa 07:00-15:00");
    assert_eq!(queued(&path, "Lark Bakery", "hours"), None);
    assert_eq!(queued(&path, "Lark Bakery", "operating_status"), None);
    assert_eq!(verdicts_and_profile(&path), before);

    let default = search(&path, json!({}), NOW);
    assert!(default.iter().all(|h| h.name != "Lark Bakery"));
    let gone = search(&path, json!({"filters": {"status": ["gone"]}}), NOW);
    let lark = hit(&gone, "Lark Bakery");
    assert_eq!(lark.verdict.as_ref().unwrap().verdict, 0);
    assert!(lark.attrs.iter().any(|a| a.key == "cuisine"));

    assert_eq!(
        queued(&path, "Fern Diner", "hours"),
        Some((1, NOW + RECHECK_FIRST_RETRY)),
        "a closing date after now writes nothing"
    );
    assert_eq!(
        hit(&default, "Fern Diner").status,
        c7::PlaceStatus::Candidate
    );

    clock.0.set(NOW + 30 * DAY);
    worker.tick();
    assert_eq!(
        queued(&path, "Lark Bakery", "hours"),
        None,
        "a gone place is not queued"
    );
}

#[test]
fn a_place_without_osm_id_stays_queued_is_never_handed_and_still_demoted() {
    let scratch = Scratch::new();
    let path = scratch.store();
    let clock = Fake::default();
    clock.0.set(NOW);
    let (src, calls) = source(|places, now| {
        Ok(places
            .iter()
            .map(|p| {
                let fresh = p.keys.iter().map(|k| {
                    let value = match k.as_str() {
                        "rating" => "4.4",
                        "operating_status" => "open",
                        _ => "Mo-Su 10:00-20:00",
                    };
                    seen(k, value, now - HOUR)
                });
                answer(p, fresh.collect(), None)
            })
            .collect())
    });
    let mut worker = scratch.worker(&clock, Some(src));
    let before = verdicts_and_profile(&path);
    let first = worker.tick();
    assert_eq!(
        first,
        NOW - HOUR + 7 * DAY + 1,
        "the earliest due_at of the fresh hours, with Moss Cellar queued adding nothing"
    );
    clock.0.set(first);
    worker.tick();

    let calls = calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    for call in &calls {
        assert!(call.iter().all(|a| a.place.0 != id(&path, "Moss Cellar")));
    }
    assert_eq!(queued(&path, "Moss Cellar", "hours"), Some((0, NOW)));
    assert_eq!(queued(&path, "Moss Cellar", "rating"), Some((0, NOW)));
    let found = search(&path, json!({}), first);
    assert_eq!(stale_marks(hit(&found, "Moss Cellar")), ["rating"]);
    assert_eq!(
        stale_marks(hit(&found, "Juniper Ramen")),
        Vec::<&str>::new()
    );
    assert_eq!(verdicts_and_profile(&path), before);
}

#[test]
fn a_value_due_before_the_next_pass_waits_for_it() {
    let scratch = Scratch::new();
    let clock = Fake::default();
    clock.0.set(NOW);
    let (src, _) = source(|places, now| {
        Ok(places
            .iter()
            .map(|p| {
                let fresh = p.keys.iter().map(|k| {
                    let value = match k.as_str() {
                        "rating" => "4.4",
                        "operating_status" => "open",
                        _ => "Mo-Su 10:00-20:00",
                    };
                    seen(k, value, now - 7 * DAY + 60)
                });
                answer(p, fresh.collect(), None)
            })
            .collect())
    });
    let mut worker = scratch.worker(&clock, Some(src));
    assert_eq!(
        worker.tick(),
        NOW + HOUR,
        "due at NOW + 61, held to the next pass"
    );
}

#[test]
fn with_the_recheck_source_none_a_pass_queues_nothing_and_adds_no_deadline() {
    let scratch = Scratch::new();
    let path = scratch.store();
    let clock = Fake::default();
    clock.0.set(NOW);
    let mut worker = scratch.worker(&clock, None);
    let before = verdicts_and_profile(&path);
    assert_eq!(worker.tick(), NOW + SCHEDULE.maintain_interval);
    let rows: i64 = Store::open(&path)
        .unwrap()
        .conn()
        .query_row("SELECT COUNT(*) FROM bed_recheck", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0);
    let found = search(&path, json!({}), NOW);
    assert!(hit_attr(hit(&found, "Juniper Ramen"), "hours").stale);
    assert_eq!(stale_marks(hit(&found, "Juniper Ramen")), ["rating"]);
    assert_eq!(verdicts_and_profile(&path), before);
}

#[test]
fn an_observation_the_bed_refuses_writes_nothing_and_its_key_backs_off() {
    let scratch = Scratch::new();
    let path = scratch.store();
    let clock = Fake::default();
    clock.0.set(NOW);
    let (src, _) = source(|places, _| {
        Ok(by_osm(places, "node/9100001")
            .map(|p| answer(p, vec![seen("rating", "very good", NOW - HOUR)], None))
            .into_iter()
            .collect())
    });
    let mut worker = scratch.worker(&clock, Some(src));
    worker.tick();
    assert_eq!(attr(&path, "Juniper Ramen", "rating").0, "4.5");
    assert_eq!(
        history(&path, "Juniper Ramen", "rating"),
        Vec::<(String, i64, i64)>::new()
    );
    assert_eq!(
        queued(&path, "Juniper Ramen", "rating"),
        Some((1, NOW + RECHECK_FIRST_RETRY))
    );
}
