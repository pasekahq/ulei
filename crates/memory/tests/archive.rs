//! The archive store through its public operations, on synthetic items in a scratch file.

#![allow(missing_docs, clippy::unwrap_used)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use ulei_memory::archive::{Archive, Caller, Error, Item, Page, Skip};
use ulei_memory::wire::c10::{ErrorCode, PutStatus, SearchRequest};

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("ulei-archive-{}-{name}", std::process::id()));
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

const CALLER: Caller = Caller {
    pid: 4242,
    uid: 1000,
};

fn item<'a>(source: &'a str, external_id: &'a str, text: &'a str, bytes: &'a [u8]) -> Item<'a> {
    Item {
        source,
        external_id,
        who: None,
        at: None,
        kind: "text/plain",
        text,
        bytes,
    }
}

fn search(q: &str) -> SearchRequest {
    SearchRequest {
        q: q.to_owned(),
        source: None,
        who: None,
        kind: None,
        since: None,
        until: None,
        limit: None,
    }
}

/// Bytes deflate cannot shorten.
fn noise(len: usize) -> Vec<u8> {
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x.to_le_bytes()[0]
        })
        .collect()
}

fn count(db: &Path, table: &str) -> i64 {
    Connection::open(db)
        .unwrap()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn stored_codec(db: &Path, id: &str) -> (String, usize) {
    let (codec, len): (String, i64) = Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT codec, length(body) FROM blobs WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    (codec, usize::try_from(len).unwrap())
}

#[test]
fn compressible_and_incompressible_originals_read_back_byte_for_byte() {
    let dir = Scratch::new("roundtrip");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let large = "The quarterly ledger lists every invoice of the orchard cooperative.\n"
        .repeat(4_000)
        .into_bytes();
    let binary = noise(64 * 1024);
    let short = b"hi".to_vec();
    let cases = [
        (
            item("notes", "ledger", "quarterly ledger orchard", &large),
            "deflate",
        ),
        (
            item("files", "photo.bin", "photo of the orchard gate", &binary),
            "none",
        ),
        (item("chat", "m-1", "hi", &short), "none"),
    ];
    for (put, codec) in &cases {
        let answer = archive.put(put).unwrap();
        assert_eq!(answer.status, PutStatus::Stored);
        let read = archive.read(&answer.id).unwrap();
        assert_eq!(read.bytes, put.bytes);
        assert_eq!(read.len, put.bytes.len() as u64);
        assert_eq!(read.kind, "text/plain");
        let (stored, body) = stored_codec(&dir.file(), &answer.id);
        assert_eq!(stored, *codec);
        if *codec == "deflate" {
            assert!(body < put.bytes.len() / 10, "{body} bytes stored");
        } else {
            assert_eq!(body, put.bytes.len());
        }
    }
    drop(archive);
    let reopened = Archive::open(&dir.file(), own()).unwrap();
    assert_eq!(reopened.items().unwrap(), 3);
}

#[test]
fn a_body_that_does_not_hash_to_its_id_is_refused_on_read() {
    let dir = Scratch::new("corrupt");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let large = "orchard ".repeat(2_000).into_bytes();
    let binary = noise(4_096);
    let deflated = archive
        .put(&item("notes", "a", "orchard", &large))
        .unwrap()
        .id;
    let raw = archive
        .put(&item("files", "b", "noise", &binary))
        .unwrap()
        .id;
    let db = Connection::open(dir.file()).unwrap();
    db.execute_batch("DROP TRIGGER blobs_immutable").unwrap();
    let other = miniz_oxide::deflate::compress_to_vec("meadows ".repeat(2_000).as_bytes(), 6);
    db.execute(
        "UPDATE blobs SET body = ?1 WHERE id = ?2",
        rusqlite::params![other, deflated],
    )
    .unwrap();
    db.execute(
        "UPDATE blobs SET body = CAST(zeroblob(4096) AS BLOB) WHERE id = ?1",
        [&raw],
    )
    .unwrap();
    for id in [&deflated, &raw] {
        let refused = archive.read(id).unwrap_err();
        assert!(matches!(refused, Error::Corrupt(_)), "{refused}");
        assert_eq!(refused.code(), ErrorCode::StoreFailed);
    }
}

#[test]
fn a_repeated_put_stores_nothing_and_answers_duplicate() {
    let dir = Scratch::new("duplicate");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let put = item(
        "chat",
        "m-7",
        "the gate code changed",
        b"the gate code changed",
    );
    let first = archive.put(&put).unwrap();
    let second = archive.put(&put).unwrap();
    assert_eq!(second.status, PutStatus::Duplicate);
    assert_eq!(second.id, first.id);
    for table in ["blobs", "entries", "item_fts"] {
        assert_eq!(count(&dir.file(), table), 1, "{table}");
    }
    assert_eq!(archive.read(&first.id).unwrap().entries.len(), 1);
}

#[test]
fn changed_bytes_under_a_stored_external_id_are_refused_as_conflict() {
    let dir = Scratch::new("conflict");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let first = archive
        .put(&item("chat", "m-7", "first words", b"first words"))
        .unwrap();
    let refused = archive
        .put(&item("chat", "m-7", "other words", b"other words"))
        .unwrap_err();
    assert!(matches!(&refused, Error::Conflict { stored } if *stored == first.id));
    assert_eq!(refused.code(), ErrorCode::Conflict);
    assert_eq!(archive.items().unwrap(), 1);
    assert_eq!(archive.read(&first.id).unwrap().bytes, b"first words");
    assert_eq!(archive.search(&search("other")).unwrap().len(), 0);
}

#[test]
fn a_new_pair_with_stored_bytes_adds_an_entry_and_no_blob() {
    let dir = Scratch::new("new-pair");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let bytes = b"minutes of the cooperative meeting";
    let first = archive
        .put(&item("mail", "msg-1", "minutes cooperative meeting", bytes))
        .unwrap();
    let second = archive
        .put(&Item {
            who: Some("treasurer"),
            kind: "message/rfc822",
            ..item("drive", "doc-9", "ignored text", bytes)
        })
        .unwrap();
    assert_eq!(second.status, PutStatus::Stored);
    assert_eq!(second.id, first.id);
    assert_eq!(count(&dir.file(), "blobs"), 1);
    assert_eq!(count(&dir.file(), "item_fts"), 1);
    let read = archive.read(&first.id).unwrap();
    let pairs: Vec<_> = read
        .entries
        .iter()
        .map(|e| (e.source.as_str(), e.external_id.as_str(), e.who.as_deref()))
        .collect();
    assert_eq!(
        pairs,
        [
            ("mail", "msg-1", None),
            ("drive", "doc-9", Some("treasurer"))
        ]
    );
    assert_eq!(read.kind, "text/plain");
}

#[test]
fn fts5_and_every_metadata_filter_find_the_item() {
    let dir = Scratch::new("filters");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let target = archive
        .put(&Item {
            who: Some("alice"),
            at: Some(1_000.0),
            kind: "text/markdown",
            ..item(
                "notes",
                "n-1",
                "Pruning the pear trees before the frost",
                b"n-1",
            )
        })
        .unwrap()
        .id;
    let other = archive
        .put(&Item {
            who: Some("bob"),
            at: Some(5_000.0),
            ..item("chat", "c-1", "The pear harvest was late", b"c-1")
        })
        .unwrap()
        .id;
    archive
        .put(&item("chat", "c-2", "The pear jam has no date", b"c-2"))
        .unwrap();
    let hits = archive.search(&search("pruning")).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, target);
    assert!(
        hits[0].snippet.contains(">>>Pruning<<<"),
        "{}",
        hits[0].snippet
    );
    assert_eq!(hits[0].entries[0].external_id, "n-1");

    let only = |request: SearchRequest| -> Vec<String> {
        archive
            .search(&request)
            .unwrap()
            .into_iter()
            .map(|h| h.id)
            .collect()
    };
    let pear = search("pear");
    assert_eq!(only(pear.clone()).len(), 3);
    let filtered = [
        SearchRequest {
            source: Some("notes".into()),
            ..pear.clone()
        },
        SearchRequest {
            who: Some("alice".into()),
            ..pear.clone()
        },
        SearchRequest {
            kind: Some("text/markdown".into()),
            ..pear.clone()
        },
        SearchRequest {
            until: Some(1_000.0),
            ..pear.clone()
        },
        SearchRequest {
            since: Some(500.0),
            until: Some(2_000.0),
            ..pear.clone()
        },
    ];
    for request in filtered {
        assert_eq!(
            only(request.clone()),
            std::slice::from_ref(&target),
            "{request:?}"
        );
    }
    assert_eq!(
        only(SearchRequest {
            since: Some(1_001.0),
            ..pear.clone()
        }),
        [other]
    );
    assert_eq!(
        only(SearchRequest {
            limit: Some(2),
            ..pear
        })
        .len(),
        2
    );
}

#[test]
fn a_query_with_fts5_syntax_characters_does_not_error() {
    let dir = Scratch::new("syntax");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let id = archive
        .put(&item(
            "notes",
            "n-1",
            "send the quarterly-report to the orchard",
            b"n-1",
        ))
        .unwrap()
        .id;
    for q in [
        "\"unbalanced",
        "(orchard",
        "orchard AND OR report",
        "NEAR(orchard",
        "text:orchard",
        "^orchard",
        "*",
        "-",
        "AND",
        "a:b:c {x} [y] <z>",
        "'; DROP TABLE blobs; --",
    ] {
        archive.search(&search(q)).unwrap();
    }
    for q in ["quarterly-report?", "(orchard)", "\"the orchard\""] {
        let hits = archive.search(&search(q)).unwrap();
        assert_eq!(hits.iter().map(|h| &h.id).collect::<Vec<_>>(), [&id], "{q}");
    }
}

#[test]
fn only_delete_removes_an_item_and_its_audit_row_is_kept() {
    let dir = Scratch::new("delete");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let bytes = b"a letter the owner wants gone";
    let id = archive
        .put(&Item {
            at: Some(42.0),
            ..item("mail", "msg-1", "letter owner gone", bytes)
        })
        .unwrap()
        .id;
    archive
        .put(&item("drive", "doc-1", "letter", bytes))
        .unwrap();
    archive
        .put(&item("mail", "msg-1", "letter", bytes))
        .unwrap();
    archive.search(&search("letter")).unwrap();
    archive.read(&id).unwrap();
    assert_eq!(archive.items().unwrap(), 1);

    let deletion = archive.delete(&id, "owner asked", CALLER).unwrap();
    assert_eq!(
        (deletion.id.as_str(), deletion.len),
        (id.as_str(), bytes.len() as u64)
    );
    assert_eq!((deletion.pid, deletion.uid), (CALLER.pid, CALLER.uid));
    assert_eq!(deletion.reason, "owner asked");
    let removed: Vec<_> = deletion
        .entries
        .iter()
        .map(|e| (e.source.as_str(), e.at))
        .collect();
    assert_eq!(removed, [("mail", Some(42.0)), ("drive", None)]);
    assert!(matches!(archive.read(&id), Err(Error::NotFound(_))));
    assert_eq!(archive.search(&search("letter")).unwrap().len(), 0);
    for table in ["blobs", "entries", "item_fts"] {
        assert_eq!(count(&dir.file(), table), 0, "{table}");
    }
    assert_eq!(
        archive.deletions().unwrap(),
        std::slice::from_ref(&deletion)
    );
    let again = archive.delete(&id, "twice", CALLER).unwrap_err();
    assert_eq!(again.code(), ErrorCode::NotFound);

    let db = Connection::open(dir.file()).unwrap();
    for sql in [
        "DELETE FROM deletions",
        "UPDATE deletions SET reason = 'edited'",
    ] {
        assert!(db.execute(sql, []).is_err(), "{sql}");
    }
    assert_eq!(archive.deletions().unwrap(), [deletion]);
}

#[test]
fn a_stored_row_cannot_be_updated() {
    let dir = Scratch::new("immutable");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let id = archive
        .put(&item("notes", "n-1", "kept as it came", b"kept as it came"))
        .unwrap()
        .id;
    let db = Connection::open(dir.file()).unwrap();
    for sql in [
        "UPDATE blobs SET len = 0",
        "UPDATE entries SET who = 'someone else'",
    ] {
        assert!(db.execute(sql, []).is_err(), "{sql}");
    }
    assert_eq!(archive.read(&id).unwrap().bytes, b"kept as it came");
}

#[test]
fn an_external_id_over_512_utf8_bytes_is_refused() {
    let dir = Scratch::new("external-id");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let at_limit = "ж".repeat(256);
    let over = "ж".repeat(257);
    assert_eq!(over.chars().count(), 257);
    archive
        .put(&item("chat", &at_limit, "fits", b"fits"))
        .unwrap();
    let refused = archive
        .put(&item("chat", &over, "too long", b"too long"))
        .unwrap_err();
    assert_eq!(refused.code(), ErrorCode::InvalidRequest);
    assert_eq!(archive.items().unwrap(), 1);
}

#[test]
fn malformed_inputs_are_invalid_requests() {
    let dir = Scratch::new("invalid");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let refusals = [
        archive.put(&item("Chat", "m-1", "x", b"x")).unwrap_err(),
        archive.put(&item("chat", "", "x", b"x")).unwrap_err(),
        archive
            .put(&Item {
                at: Some(-1.0),
                ..item("chat", "m-1", "x", b"x")
            })
            .unwrap_err(),
        archive.search(&search("")).unwrap_err(),
        archive
            .search(&SearchRequest {
                limit: Some(51),
                ..search("x")
            })
            .unwrap_err(),
        archive.read("ABC").unwrap_err(),
        archive.delete(&"0".repeat(64), "", CALLER).unwrap_err(),
    ];
    for refused in refusals {
        assert_eq!(refused.code(), ErrorCode::InvalidRequest, "{refused}");
    }
    assert_eq!(
        archive.read(&"0".repeat(64)).unwrap_err().code(),
        ErrorCode::NotFound
    );
}

#[test]
fn the_file_is_0600_in_a_0700_directory_in_wal_mode() {
    let dir = Scratch::new("modes");
    let archive = Archive::open(&dir.file(), own()).unwrap();
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&dir.file()), 0o600);
    assert_eq!(mode(dir.file().parent().unwrap()), 0o700);
    let journal_mode: String = Connection::open(dir.file())
        .unwrap()
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(journal_mode, "wal");
    drop(archive);
}

#[test]
fn a_page_with_its_skips_and_cursor_is_stored_whole_or_not_at_all() {
    let dir = Scratch::new("page");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    archive
        .put(&item("dialogue", "owner/1/1", "kept", b"old"))
        .unwrap();
    let skip = Skip {
        external_id: "owner/1/2",
        len: 900_000,
        at: Some(1_700_000_000.0),
    };
    let mut page = Page {
        source: "dialogue",
        items: vec![
            item("dialogue", "owner/1/0", "tulips", b"first"),
            item("dialogue", "owner/1/1", "kept", b"changed"),
        ],
        skipped: vec![skip],
        cursor: Some(3),
    };
    assert!(matches!(archive.pull(&page), Err(Error::Conflict { .. })));
    assert_eq!(archive.cursor("dialogue").unwrap(), None);
    assert_eq!(archive.items().unwrap(), 1);
    assert_eq!(archive.skipped().unwrap(), []);

    page.items[1] = item("dialogue", "owner/1/1", "kept", b"old");
    assert_eq!(archive.pull(&page).unwrap(), 1);
    assert_eq!(
        archive.pull(&page).unwrap(),
        0,
        "a repeated page adds nothing"
    );
    assert_eq!(archive.cursor("dialogue").unwrap(), Some(3));
    assert_eq!(archive.items().unwrap(), 2);
    let kept = archive.skipped().unwrap();
    assert_eq!(kept.len(), 1);
    assert_eq!(
        (kept[0].external_id.as_str(), kept[0].len, kept[0].at),
        ("owner/1/2", 900_000, Some(1_700_000_000.0))
    );

    page.cursor = None;
    page.items.clear();
    archive.pull(&page).unwrap();
    assert_eq!(
        archive.cursor("dialogue").unwrap(),
        Some(3),
        "a page without a cursor keeps the stored one"
    );
}

#[test]
fn a_store_directory_of_another_uid_is_refused_and_nothing_is_created() {
    let dir = Scratch::new("foreign");
    fs::create_dir_all(dir.file().parent().unwrap()).unwrap();
    fs::set_permissions(
        dir.file().parent().unwrap(),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let other = own().wrapping_add(1);
    let refused = Archive::open(&dir.file(), other).unwrap_err();
    assert!(
        matches!(&refused, Error::ForeignOwner { uid, .. } if *uid == own()),
        "{refused}"
    );
    assert!(refused.to_string().contains("belongs to uid"), "{refused}");
    assert!(!dir.file().exists());
}

#[test]
fn a_version_1_file_gains_the_skipped_table_and_keeps_its_items() {
    let dir = Scratch::new("upgrade");
    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    let id = archive
        .put(&item("notes", "note-1", "orchard", b"kept"))
        .unwrap()
        .id;
    drop(archive);
    Connection::open(dir.file())
        .unwrap()
        .execute_batch("DROP TABLE skipped; PRAGMA user_version = 1;")
        .unwrap();

    let mut archive = Archive::open(&dir.file(), own()).unwrap();
    assert_eq!(archive.read(&id).unwrap().bytes, b"kept");
    let page = Page {
        source: "dialogue",
        items: vec![],
        skipped: vec![Skip {
            external_id: "owner/1/0",
            len: 1,
            at: None,
        }],
        cursor: Some(1),
    };
    archive.pull(&page).unwrap();
    assert_eq!(archive.skipped().unwrap().len(), 1);
}
