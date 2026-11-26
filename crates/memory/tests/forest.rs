//! The forest (ADR-0079) through the archive's public operations, on synthetic items and records
//! in a scratch file: a put and its search, the replace of a rerun, a continuation kept beside
//! its first part, links checked against the archive, forget and undo, and a version 4 file
//! gaining the forest's tables without a rewrite.

#![allow(missing_docs, clippy::unwrap_used)]

use std::fs;
use std::path::PathBuf;

use rusqlite::Connection;
use ulei_memory::archive::forest::{MAX_FOREST_LIMIT, MAX_LINKS};
use ulei_memory::archive::{Archive, Error, Item};
use ulei_memory::wire::c10::{
    ForestForgetRequest, ForestLink, ForestPutRequest, ForestSearchRequest, ListRequest, Producer,
    PutStatus,
};

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("ulei-forest-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn file(&self) -> PathBuf {
        self.0.join("store").join("memory.archive")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn own() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// An archive holding the messages `s-1/m-1` to `s-1/m-4` and `s-2/m-1` of source `old-runtime`
/// and one item of source `notes`; answers it with each entry's `(seq, id)` by external id.
fn archive(dir: &Scratch) -> (Archive, Vec<(String, u64, String)>) {
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    for (source, external_id) in [
        ("old-runtime", "s-1/m-1"),
        ("old-runtime", "s-1/m-2"),
        ("old-runtime", "s-1/m-3"),
        ("old-runtime", "s-1/m-4"),
        ("old-runtime", "s-2/m-1"),
        ("notes", "n-1"),
    ] {
        let text = format!("{external_id}: Ada Example talks about the garden shed.");
        archive
            .put(&Item {
                source,
                external_id,
                who: Some("user"),
                at: Some(1_700_000_000.0),
                kind: "text/plain",
                text: &text,
                bytes: text.as_bytes(),
            })
            .unwrap();
    }
    let mut entries = Vec::new();
    for source in ["old-runtime", "notes"] {
        let page = archive
            .list(&ListRequest {
                source: source.to_owned(),
                after: None,
                limit: None,
                text_chars: None,
            })
            .unwrap();
        entries.extend(
            page.entries
                .into_iter()
                .map(|e| (e.external_id, e.seq, e.id)),
        );
    }
    (archive, entries)
}

fn link(entries: &[(String, u64, String)], external_id: &str) -> ForestLink {
    let (_, seq, id) = entries.iter().find(|e| e.0 == external_id).unwrap();
    ForestLink {
        seq: *seq,
        id: id.clone(),
    }
}

fn record(
    entries: &[(String, u64, String)],
    session: &str,
    text: &str,
    messages: &[&str],
) -> ForestPutRequest {
    ForestPutRequest {
        source: "old-runtime".to_owned(),
        session: session.to_owned(),
        text: text.to_owned(),
        from: Some(1_700_000_000.0),
        to: Some(1_700_000_300.0),
        links: messages.iter().map(|m| link(entries, m)).collect(),
        producer: Producer {
            run: "1700100000-42".to_owned(),
            route: "small".to_owned(),
        },
    }
}

fn search(q: &str) -> ForestSearchRequest {
    ForestSearchRequest {
        q: q.to_owned(),
        source: None,
        since: None,
        until: None,
        limit: None,
    }
}

fn found(archive: &Archive, q: &str) -> Vec<u64> {
    archive
        .forest_search(&search(q))
        .unwrap()
        .iter()
        .map(|h| h.id)
        .collect()
}

fn forget(ids: &[u64], undo: bool) -> ForestForgetRequest {
    ForestForgetRequest {
        ids: ids.to_vec(),
        reason: (!undo).then(|| "The owner asked.".to_owned()),
        provenance: "s1-t2-n3".to_owned(),
        undo: undo.then_some(true),
    }
}

#[test]
fn a_record_is_found_with_its_span_producer_and_links_and_filters_narrow_it() {
    let dir = Scratch::new("put-search");
    let (mut archive, entries) = archive(&dir);
    let put = archive
        .forest_put(&record(
            &entries,
            "s-1",
            "Ada Example planned the move to Sample Town.",
            &["s-1/m-1", "s-1/m-3"],
        ))
        .unwrap();
    assert_eq!(
        (put.id, put.status, put.replaced),
        (1, PutStatus::Stored, None)
    );

    let hits = archive.forest_search(&search("sample town")).unwrap();
    assert_eq!(hits.len(), 1);
    let hit = &hits[0];
    assert_eq!(
        (hit.id, hit.source.as_str(), hit.session.as_str()),
        (1, "old-runtime", "s-1")
    );
    assert_eq!(hit.text, "Ada Example planned the move to Sample Town.");
    assert!(hit.snippet.contains(">>>Sample<<<"), "{}", hit.snippet);
    assert_eq!(
        (hit.from, hit.to),
        (Some(1_700_000_000.0), Some(1_700_000_300.0))
    );
    assert_eq!(hit.producer.run, "1700100000-42");
    assert_eq!(hit.producer.route, "small");
    let linked: Vec<(&str, &str)> = hit
        .links
        .iter()
        .map(|l| (l.external_id.as_str(), l.id.as_str()))
        .collect();
    assert_eq!(
        linked,
        [
            ("s-1/m-1", link(&entries, "s-1/m-1").id.as_str()),
            ("s-1/m-3", link(&entries, "s-1/m-3").id.as_str())
        ]
    );

    let narrowed = |request: ForestSearchRequest| archive.forest_search(&request).unwrap().len();
    let source = |s: &str| ForestSearchRequest {
        source: Some(s.to_owned()),
        ..search("town")
    };
    assert_eq!(narrowed(source("old-runtime")), 1);
    assert_eq!(narrowed(source("notes")), 0);
    let span = |since: Option<f64>, until: Option<f64>| ForestSearchRequest {
        since,
        until,
        ..search("town")
    };
    assert_eq!(narrowed(span(Some(1_700_000_300.0), None)), 1);
    assert_eq!(narrowed(span(Some(1_700_000_301.0), None)), 0);
    assert_eq!(narrowed(span(None, Some(1_700_000_000.0))), 1);
    assert_eq!(narrowed(span(None, Some(1_699_999_999.0))), 0);
    assert_eq!(found(&archive, "AND OR \"unbalanced ("), Vec::<u64>::new());
    assert_eq!(archive.forest_records().unwrap(), 1);
}

#[test]
fn a_rerun_of_a_session_replaces_its_record_and_a_continuation_stays_beside_it() {
    let dir = Scratch::new("replace");
    let (mut archive, entries) = archive(&dir);
    let first = record(
        &entries,
        "s-1",
        "First summary of the shed.",
        &["s-1/m-1", "s-1/m-2"],
    );
    assert_eq!(archive.forest_put(&first).unwrap().id, 1);

    // The same record again changes nothing.
    let again = archive.forest_put(&first).unwrap();
    assert_eq!((again.id, again.status), (1, PutStatus::Duplicate));

    // A rerun of the session with another text and a split that overlaps the first.
    let rerun = record(
        &entries,
        "s-1",
        "Second summary of the shed.",
        &["s-1/m-2", "s-1/m-3"],
    );
    let put = archive.forest_put(&rerun).unwrap();
    assert_eq!(
        (put.id, put.status, put.replaced),
        (2, PutStatus::Stored, Some(vec![1]))
    );
    assert_eq!(found(&archive, "shed"), [2]);

    // A continuation on a later night links later messages only and replaces nothing.
    let more = record(&entries, "s-1", "Third part about the shed.", &["s-1/m-4"]);
    let put = archive.forest_put(&more).unwrap();
    assert_eq!((put.id, put.replaced), (3, None));
    // A record of another session replaces nothing of this one.
    let other = record(
        &entries,
        "s-2",
        "Other session about the shed.",
        &["s-2/m-1"],
    );
    assert_eq!(archive.forest_put(&other).unwrap().replaced, None);
    let mut ids = found(&archive, "shed");
    ids.sort_unstable();
    assert_eq!(ids, [2, 3, 4]);
    assert_eq!(archive.forest_records().unwrap(), 3);

    // The superseded record is kept in the file.
    drop(archive);
    let conn = Connection::open(dir.file()).unwrap();
    let kept: i64 = conn
        .query_row("SELECT count(*) FROM forest", [], |r| r.get(0))
        .unwrap();
    assert_eq!(kept, 4);
    assert!(conn.execute("UPDATE forest SET text = 'x'", []).is_err());
    assert!(conn.execute("DELETE FROM forest_superseded", []).is_err());
}

#[test]
fn links_must_name_stored_entries_of_the_records_source() {
    let dir = Scratch::new("links");
    let (mut archive, entries) = archive(&dir);
    let mut wrong_source = record(&entries, "s-1", "Shed.", &["s-1/m-1"]);
    wrong_source.links = vec![link(&entries, "n-1")];
    assert!(matches!(
        archive.forest_put(&wrong_source),
        Err(Error::NotFound(_))
    ));
    let mut wrong_id = record(&entries, "s-1", "Shed.", &["s-1/m-1"]);
    wrong_id.links[0].id = link(&entries, "s-1/m-2").id;
    assert!(matches!(
        archive.forest_put(&wrong_id),
        Err(Error::Invalid(_))
    ));
    let mut twice = record(&entries, "s-1", "Shed.", &["s-1/m-1"]);
    twice.links.push(twice.links[0].clone());
    assert!(matches!(archive.forest_put(&twice), Err(Error::Invalid(_))));
    let mut no_span_end = record(&entries, "s-1", "Shed.", &["s-1/m-1"]);
    no_span_end.to = None;
    assert!(matches!(
        archive.forest_put(&no_span_end),
        Err(Error::Invalid(_))
    ));
    let mut bad_run = record(&entries, "s-1", "Shed.", &["s-1/m-1"]);
    bad_run.producer.run = "tonight".to_owned();
    assert!(matches!(
        archive.forest_put(&bad_run),
        Err(Error::Invalid(_))
    ));
    let mut many = record(&entries, "s-1", "Shed.", &["s-1/m-1"]);
    many.links = (1..=MAX_LINKS as u64 + 1)
        .map(|seq| ForestLink {
            seq,
            id: "0".repeat(64),
        })
        .collect();
    assert!(matches!(archive.forest_put(&many), Err(Error::Invalid(_))));
    let over = ForestSearchRequest {
        limit: Some(MAX_FOREST_LIMIT + 1),
        ..search("shed")
    };
    assert!(matches!(
        archive.forest_search(&over),
        Err(Error::Invalid(_))
    ));
    assert_eq!(archive.forest_records().unwrap(), 0);
}

#[test]
fn a_forgotten_record_leaves_search_until_its_mark_is_lifted_and_a_replace_keeps_the_mark() {
    let dir = Scratch::new("forget");
    let (mut archive, entries) = archive(&dir);
    archive
        .forest_put(&record(&entries, "s-1", "The shed key.", &["s-1/m-1"]))
        .unwrap();
    let marked = archive.forest_forget(&forget(&[1, 1], false)).unwrap();
    let marks: Vec<(u64, u64, u64)> = marked
        .items
        .iter()
        .map(|i| (i.id, i.record, i.marks))
        .collect();
    assert_eq!(marks, [(1, 1, 1), (1, 1, 0)], "no second live mark");
    assert_eq!(found(&archive, "shed"), Vec::<u64>::new());

    // A rerun replaces the forgotten record: the new one is forgotten too.
    let put = archive
        .forest_put(&record(
            &entries,
            "s-1",
            "The shed key again.",
            &["s-1/m-1"],
        ))
        .unwrap();
    assert_eq!(put.replaced, Some(vec![1]));
    assert_eq!(found(&archive, "shed"), Vec::<u64>::new());

    // Undo by the old id reaches the current version.
    let lifted = archive.forest_forget(&forget(&[1], true)).unwrap();
    assert_eq!((lifted.items[0].record, lifted.items[0].marks), (put.id, 1));
    assert_eq!(found(&archive, "shed"), [put.id]);
    let none = archive.forest_forget(&forget(&[put.id], true)).unwrap();
    assert_eq!(none.items[0].marks, 0);

    // An unknown id changes nothing.
    assert!(matches!(
        archive.forest_forget(&forget(&[put.id, 99], false)),
        Err(Error::NotFound(_))
    ));
    assert_eq!(found(&archive, "shed"), [put.id]);
    let mut no_reason = forget(&[put.id], false);
    no_reason.reason = None;
    assert!(matches!(
        archive.forest_forget(&no_reason),
        Err(Error::Invalid(_))
    ));
    let mut bad = forget(&[put.id], false);
    bad.provenance = "owner".to_owned();
    assert!(matches!(
        archive.forest_forget(&bad),
        Err(Error::Invalid(_))
    ));
    drop(archive);

    let conn = Connection::open(dir.file()).unwrap();
    let marks: Vec<(i64, String, String, Option<String>)> = conn
        .prepare(
            "SELECT record, reason, provenance, lifted_provenance FROM forest_marks ORDER BY row",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        marks,
        [
            (1, "The owner asked.".into(), "s1-t2-n3".into(), None),
            (
                2,
                "The owner asked.".into(),
                "s1-t2-n3".into(),
                Some("s1-t2-n3".into())
            ),
        ]
    );
    assert!(conn.execute("DELETE FROM forest_marks", []).is_err());
    assert!(
        conn.execute(
            "UPDATE forest_marks SET lifted_at = NULL WHERE record = 2",
            []
        )
        .is_err(),
        "a lift is never undone in place"
    );
}

#[test]
fn a_deleted_archive_item_leaves_the_links_of_a_hit() {
    let dir = Scratch::new("deleted");
    let (mut archive, entries) = archive(&dir);
    archive
        .forest_put(&record(
            &entries,
            "s-1",
            "The shed.",
            &["s-1/m-1", "s-1/m-2"],
        ))
        .unwrap();
    let gone = link(&entries, "s-1/m-1").id;
    archive
        .delete(
            &gone,
            "owner asked",
            ulei_memory::archive::Caller { pid: 1, uid: 1 },
        )
        .unwrap();
    let hits = archive.forest_search(&search("shed")).unwrap();
    let left: Vec<&str> = hits[0]
        .links
        .iter()
        .map(|l| l.external_id.as_str())
        .collect();
    assert_eq!(left, ["s-1/m-2"]);
}

#[test]
fn a_version_4_file_gains_the_forest_without_a_rewrite() {
    let dir = Scratch::new("upgrade-4");
    let (archive, entries) = archive(&dir);
    drop(archive);
    let conn = Connection::open(dir.file()).unwrap();
    conn.execute_batch(
        "DROP TABLE forest_marks; DROP TABLE forest_fts; DROP TABLE forest_superseded; \
         DROP TABLE forest_links; DROP TABLE forest; PRAGMA user_version = 4;",
    )
    .unwrap();
    let dump = |conn: &Connection| -> Vec<(String, String, f64)> {
        conn.prepare("SELECT external_id, id, put_at FROM entries ORDER BY rowid")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let before = dump(&conn);
    drop(conn);

    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    assert_eq!(archive.items().unwrap(), 6);
    archive
        .forest_put(&record(&entries, "s-1", "The shed.", &["s-1/m-1"]))
        .unwrap();
    assert_eq!(found(&archive, "shed"), [1]);
    drop(archive);
    let conn = Connection::open(dir.file()).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 5);
    assert_eq!(dump(&conn), before);
}
